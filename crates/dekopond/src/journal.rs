use std::{
    collections::HashMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dekopon_agent::prompt::{ConversationTurn, History};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    asset::{AssetRef, AssetSourceRef, MAX_ASSETS_PER_CONVERSATION, PendingAsset, RecalledAsset},
    config::MemoryWindow,
};

const FORMAT_VERSION: u32 = 1;
const EXTENSION: &str = "jsonl";
const WHATSAPP_MEDIA_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const LINE_SLACK_BYTES: u64 = 64 * 1024;

#[derive(Debug, Error)]
pub(crate) enum JournalError {
    #[error("could not read or write the conversation journal")]
    Io { kind: io::ErrorKind },
    #[error("a conversation journal file did not parse and was discarded")]
    Corrupt,
}

impl From<io::Error> for JournalError {
    fn from(error: io::Error) -> Self {
        Self::Io { kind: error.kind() }
    }
}

impl JournalError {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Io { .. } => "io",
            Self::Corrupt => "corrupt",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Line {
    v: u32,
    at_ms: u64,
    grant: String,
    user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answer: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assets: Vec<LineAsset>,
    // Unjournaled assets still consumed their numbers, and a later arrival must not reuse one.
    #[serde(default, skip_serializing_if = "is_zero")]
    last_asset_id: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn last_asset_id(lines: &[Line]) -> Option<u64> {
    lines
        .iter()
        .flat_map(|line| {
            line.assets
                .iter()
                .map(|asset| asset.id)
                .chain([line.last_asset_id])
        })
        .max()
        .filter(|id| *id > 0)
}

impl Line {
    fn turn_bytes(&self) -> usize {
        self.user
            .len()
            .saturating_add(self.answer.as_ref().map_or(0, String::len))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LineAsset {
    id: u64,
    name: String,
    mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    source: LineSource,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum LineSource {
    Slack {
        file_id: String,
        url: String,
    },
    Discord {
        attachment_id: String,
        channel_id: String,
        message_id: String,
        url: String,
    },
    WhatsApp {
        media_id: String,
        mime: String,
    },
    Telegram {
        file_id: String,
    },
}

impl LineSource {
    // Generated outputs live in the broker's spool and do not survive the session that made them.
    fn from_ref(source: &AssetSourceRef) -> Option<Self> {
        match source {
            AssetSourceRef::Generated { .. } => None,
            AssetSourceRef::Slack { file_id, url } => Some(Self::Slack {
                file_id: file_id.clone(),
                url: url.clone(),
            }),
            AssetSourceRef::Discord {
                attachment_id,
                channel_id,
                message_id,
                url,
            } => Some(Self::Discord {
                attachment_id: attachment_id.clone(),
                channel_id: channel_id.clone(),
                message_id: message_id.clone(),
                url: url.clone(),
            }),
            AssetSourceRef::WhatsApp { media_id, mime } => Some(Self::WhatsApp {
                media_id: media_id.clone(),
                mime: mime.clone(),
            }),
            AssetSourceRef::Telegram { file_id } => Some(Self::Telegram {
                file_id: file_id.clone(),
            }),
        }
    }

    fn into_ref(self) -> AssetSourceRef {
        match self {
            Self::Slack { file_id, url } => AssetSourceRef::Slack { file_id, url },
            Self::Discord {
                attachment_id,
                channel_id,
                message_id,
                url,
            } => AssetSourceRef::Discord {
                attachment_id,
                channel_id,
                message_id,
                url,
            },
            Self::WhatsApp { media_id, mime } => AssetSourceRef::WhatsApp { media_id, mime },
            Self::Telegram { file_id } => AssetSourceRef::Telegram { file_id },
        }
    }
}

pub(crate) struct Recalled {
    pub history: History,
    pub assets: Vec<RecalledAsset>,
    pub next_asset_id: u64,
}

pub(crate) struct Entry<'a> {
    pub at: SystemTime,
    pub grant: &'a str,
    pub turn: &'a ConversationTurn,
    pub inventory: &'a [AssetRef],
}

struct FileState {
    bytes: u64,
    touched: u64,
    max_asset_id: Option<u64>,
}

struct Files {
    entries: HashMap<String, FileState>,
    total: u64,
    clock: u64,
}

impl Files {
    fn tick(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }
}

// Every method does blocking file I/O under `files`; callers run it on a blocking thread, and the
// one lock is what orders appends, compaction and eviction for the whole directory.
pub(crate) struct Journal {
    dir: PathBuf,
    max_bytes: u64,
    files: Mutex<Files>,
}

impl Journal {
    pub fn open(dir: &Path, max_bytes: u64) -> Result<Self, JournalError> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let mut found = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = entry.metadata()?;
            if !metadata.is_file()
                || path
                    .extension()
                    .is_none_or(|extension| extension != EXTENSION)
            {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            found.push((
                stem.to_owned(),
                metadata.len(),
                metadata.modified().unwrap_or(UNIX_EPOCH),
            ));
        }
        found.sort_by_key(|(_, _, modified)| *modified);
        let mut files = Files {
            entries: HashMap::with_capacity(found.len()),
            total: 0,
            clock: 0,
        };
        for (stem, bytes, _) in found {
            let touched = files.tick();
            files.total = files.total.saturating_add(bytes);
            files.entries.insert(
                stem,
                FileState {
                    bytes,
                    touched,
                    max_asset_id: None,
                },
            );
        }
        let journal = Self {
            dir: dir.to_path_buf(),
            max_bytes,
            files: Mutex::new(files),
        };
        {
            let mut files = journal.files.lock().expect("journal files");
            journal.enforce_total(&mut files, None);
        }
        Ok(journal)
    }

    pub fn recall(
        &self,
        stem: &str,
        grant: &str,
        window: MemoryWindow,
        now: SystemTime,
    ) -> Result<Recalled, JournalError> {
        let mut files = self.files.lock().expect("journal files");
        let path = self.path(stem);
        let lines = match read_lines(&path, read_limit(window)) {
            Ok(lines) => lines,
            Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            }) => {
                files.entries.remove(stem);
                return Ok(Recalled {
                    history: History::new(window.limits),
                    assets: Vec::new(),
                    next_asset_id: 1,
                });
            }
            Err(JournalError::Corrupt) => {
                self.discard(&mut files, stem);
                return Err(JournalError::Corrupt);
            }
            Err(error) => return Err(error),
        };
        let max_asset_id = last_asset_id(&lines);
        let touched = files.tick();
        let bytes = fs::metadata(&path).map_or(0, |metadata| metadata.len());
        let previous = files.entries.insert(
            stem.to_owned(),
            FileState {
                bytes,
                touched,
                max_asset_id: Some(max_asset_id.unwrap_or(0)),
            },
        );
        files.total = files
            .total
            .saturating_sub(previous.map_or(0, |state| state.bytes))
            .saturating_add(bytes);

        let now_ms = millis(now);
        let horizon_ms = now_ms.saturating_sub(duration_millis(window.forget_after));
        let media_horizon_ms = now_ms.saturating_sub(duration_millis(WHATSAPP_MEDIA_LIFETIME));
        // Only the trailing run under this grant is recalled, so a narrowed grant never replays
        // output produced under a wider one.
        let current = lines
            .iter()
            .rev()
            .take_while(|line| line.grant == grant)
            .count();
        let kept = lines
            .into_iter()
            .rev()
            .take(current)
            .rev()
            .filter(|line| line.at_ms >= horizon_ms)
            .collect::<Vec<_>>();

        let mut assets = Vec::new();
        for line in &kept {
            for asset in &line.assets {
                let source = match &asset.source {
                    LineSource::WhatsApp { .. } if line.at_ms < media_horizon_ms => None,
                    source => Some(source.clone().into_ref()),
                };
                assets.push(RecalledAsset {
                    id: asset.id,
                    asset: PendingAsset {
                        name: asset.name.clone(),
                        mime: asset.mime.clone(),
                        size: asset.size,
                        source,
                    },
                });
            }
        }
        let excess = assets.len().saturating_sub(MAX_ASSETS_PER_CONVERSATION);
        assets.drain(..excess);
        let history = History::from_turns(
            window.limits,
            kept.into_iter().map(|line| match line.answer {
                Some(answer) => ConversationTurn::completed(line.user, answer),
                None => ConversationTurn::unanswered(line.user),
            }),
        );
        Ok(Recalled {
            history,
            assets,
            next_asset_id: max_asset_id.map_or(1, |id| id.saturating_add(1)),
        })
    }

    pub fn append(
        &self,
        stem: &str,
        entry: &Entry<'_>,
        window: MemoryWindow,
    ) -> Result<(), JournalError> {
        let mut files = self.files.lock().expect("journal files");
        let path = self.path(stem);
        let known = match files.entries.get(stem).and_then(|state| state.max_asset_id) {
            Some(max) => max,
            None => match read_lines(&path, read_limit(window)) {
                Ok(lines) => last_asset_id(&lines).unwrap_or(0),
                Err(JournalError::Io {
                    kind: io::ErrorKind::NotFound,
                }) => 0,
                Err(JournalError::Corrupt) => {
                    self.discard(&mut files, stem);
                    0
                }
                Err(error) => return Err(error),
            },
        };
        let assets = entry
            .inventory
            .iter()
            .filter(|asset| asset.id > known)
            .filter_map(|asset| {
                Some(LineAsset {
                    id: asset.id,
                    name: asset.name.clone(),
                    mime: asset.mime.clone(),
                    size: asset.size,
                    source: LineSource::from_ref(asset.source.as_ref()?)?,
                })
            })
            .collect::<Vec<_>>();
        let max_asset_id = entry
            .inventory
            .iter()
            .map(|asset| asset.id)
            .max()
            .unwrap_or(0)
            .max(known);
        let line = Line {
            v: FORMAT_VERSION,
            at_ms: millis(entry.at),
            grant: entry.grant.to_owned(),
            user: entry.turn.user().to_owned(),
            answer: entry.turn.answer().map(str::to_owned),
            assets,
            last_asset_id: max_asset_id,
        };
        let mut encoded = serde_json::to_vec(&line).map_err(io::Error::from)?;
        encoded.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&encoded)?;
        drop(file);

        let mut bytes = fs::metadata(&path)?.len();
        if bytes > compaction_threshold(window) {
            bytes = compact(&path, window, read_limit(window))?;
        }
        let touched = files.tick();
        let previous = files.entries.insert(
            stem.to_owned(),
            FileState {
                bytes,
                touched,
                max_asset_id: Some(max_asset_id),
            },
        );
        files.total = files
            .total
            .saturating_sub(previous.map_or(0, |state| state.bytes))
            .saturating_add(bytes);
        self.enforce_total(&mut files, Some(stem));
        Ok(())
    }

    fn path(&self, stem: &str) -> PathBuf {
        self.dir.join(format!("{stem}.{EXTENSION}"))
    }

    fn discard(&self, files: &mut Files, stem: &str) {
        if let Err(error) = fs::remove_file(self.path(stem))
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(event = "gateway_journal_remove_failed", kind = %error.kind());
        }
        if let Some(state) = files.entries.remove(stem) {
            files.total = files.total.saturating_sub(state.bytes);
        }
    }

    fn enforce_total(&self, files: &mut Files, keep: Option<&str>) {
        while files.total > self.max_bytes {
            let Some(oldest) = files
                .entries
                .iter()
                .filter(|(stem, _)| Some(stem.as_str()) != keep)
                .min_by_key(|(_, state)| state.touched)
                .map(|(stem, _)| stem.clone())
            else {
                return;
            };
            self.discard(files, &oldest);
            tracing::info!(event = "gateway_journal_evicted", reason = "capacity");
        }
    }
}

pub(crate) fn grant_digest(granted: &[String]) -> String {
    let mut digest = Sha256::new();
    for capability in granted {
        digest.update((capability.len() as u64).to_be_bytes());
        digest.update(capability.as_bytes());
    }
    digest
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn compaction_threshold(window: MemoryWindow) -> u64 {
    (window.limits.max_bytes as u64)
        .saturating_mul(2)
        .saturating_add(LINE_SLACK_BYTES)
}

fn read_limit(window: MemoryWindow) -> u64 {
    compaction_threshold(window).saturating_mul(2)
}

fn millis(at: SystemTime) -> u64 {
    duration_millis(at.duration_since(UNIX_EPOCH).unwrap_or_default())
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

// A file larger than `limit` is read from its tail; the first, partial line is skipped.
fn read_lines(path: &Path, limit: u64) -> Result<Vec<Line>, JournalError> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let skip_partial = length > limit;
    if skip_partial {
        file.seek(SeekFrom::Start(length - limit))?;
    }
    let mut reader = BufReader::new(file.take(limit));
    if skip_partial {
        let mut discarded = Vec::new();
        reader.read_until(b'\n', &mut discarded)?;
    }
    let mut lines = Vec::new();
    let mut buffer = String::new();
    loop {
        buffer.clear();
        if reader.read_line(&mut buffer)? == 0 {
            break;
        }
        if !buffer.ends_with('\n') {
            return Err(JournalError::Corrupt);
        }
        let line: Line =
            serde_json::from_str(&buffer).map_err(|_malformed| JournalError::Corrupt)?;
        if line.v != FORMAT_VERSION {
            return Err(JournalError::Corrupt);
        }
        lines.push(line);
    }
    Ok(lines)
}

// Keeps the newest lines the window could replay; assets named only by dropped lines move onto
// the oldest kept line, because every later turn's reference note still lists them.
fn compact(path: &Path, window: MemoryWindow, limit: u64) -> Result<u64, JournalError> {
    let mut lines = read_lines(path, limit)?;
    let mut kept = 0;
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        bytes = bytes.saturating_add(line.turn_bytes());
        if kept > 0 && (kept >= window.limits.max_turns || bytes > window.limits.max_bytes) {
            break;
        }
        kept += 1;
    }
    let dropped = lines.drain(..lines.len() - kept).collect::<Vec<_>>();
    let mut carried = dropped
        .into_iter()
        .flat_map(|line| line.assets)
        .collect::<Vec<_>>();
    if let Some(first) = lines.first_mut() {
        carried.append(&mut first.assets);
        carried.sort_by_key(|asset| asset.id);
        let excess = carried.len().saturating_sub(MAX_ASSETS_PER_CONVERSATION);
        carried.drain(..excess);
        first.assets = carried;
    }
    let temporary = path.with_extension("compact");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    for line in &lines {
        let mut encoded = serde_json::to_vec(line).map_err(io::Error::from)?;
        encoded.push(b'\n');
        file.write_all(&encoded)?;
    }
    drop(file);
    fs::rename(&temporary, path)?;
    Ok(fs::metadata(path)?.len())
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Instant};

    use dekopon_agent::prompt::HistoryLimits;

    use super::*;
    use crate::{
        asset::{AssetAccess, AssetFence, AssetStore},
        config::{MemoryScope, RecallSource},
        conversation::ConversationKey,
    };

    const GRANT: &str = "aaaaaaaaaaaaaaaa";

    fn window() -> MemoryWindow {
        MemoryWindow {
            scope: MemoryScope::PrivateConversation,
            idle_timeout: Duration::from_secs(300),
            limits: HistoryLimits {
                max_turns: 4,
                max_bytes: 1024,
            },
            recall: RecallSource::Journal,
            forget_after: Duration::from_secs(3600),
        }
    }

    fn key() -> ConversationKey {
        ConversationKey::private(
            &"reviewer".parse().expect("agent fixture"),
            "whatsapp-test",
            "15550001111",
            &"tel.16034700182".parse().expect("subject fixture"),
        )
    }

    fn append(journal: &Journal, stem: &str, at: SystemTime, grant: &str, user: &str) {
        append_with(journal, stem, at, grant, user, &[]);
    }

    fn append_with(
        journal: &Journal,
        stem: &str,
        at: SystemTime,
        grant: &str,
        user: &str,
        inventory: &[AssetRef],
    ) {
        let turn = ConversationTurn::completed(user, format!("re: {user}"));
        journal
            .append(
                stem,
                &Entry {
                    at,
                    grant,
                    turn: &turn,
                    inventory,
                },
                window(),
            )
            .expect("append");
    }

    fn photos(count: usize, source: impl Fn(usize) -> AssetSourceRef) -> Vec<AssetRef> {
        let store = AssetStore::new(8, Duration::from_secs(3600));
        let access = AssetAccess::persistent(key(), 1, Arc::new(AssetFence::new()));
        let pending = (0..count)
            .map(|index| PendingAsset {
                name: format!("photo-{index}.jpg"),
                mime: "image/jpeg".to_owned(),
                size: Some(1000),
                source: Some(source(index)),
            })
            .collect();
        store
            .assets_for_access(&access, pending, true, Instant::now())
            .inventory
    }

    fn whatsapp(index: usize) -> AssetSourceRef {
        AssetSourceRef::WhatsApp {
            media_id: format!("media-{index}"),
            mime: "image/jpeg".to_owned(),
        }
    }

    fn users(recalled: &Recalled) -> Vec<&str> {
        recalled
            .history
            .turns()
            .iter()
            .map(ConversationTurn::user)
            .collect()
    }

    #[test]
    fn a_reopened_journal_recalls_the_window_written_before_the_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        {
            let journal = Journal::open(dir.path(), 1 << 20).expect("open");
            append(&journal, &stem, now, GRANT, "one");
            append(&journal, &stem, now, GRANT, "two");
        }
        let journal = Journal::open(dir.path(), 1 << 20).expect("reopen");
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        assert_eq!(users(&recalled), ["one", "two"]);
        assert_eq!(recalled.history.turns()[1].answer(), Some("re: two"));
    }

    #[test]
    fn a_conversation_with_no_file_recalls_an_empty_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), 1 << 20).expect("open");
        let recalled = journal
            .recall(&key().journal_stem(), GRANT, window(), SystemTime::now())
            .expect("recall");
        assert!(recalled.history.is_empty());
        assert_eq!(recalled.next_asset_id, 1);
    }

    #[test]
    fn only_the_trailing_run_under_the_current_grant_is_recalled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), 1 << 20).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        append(&journal, &stem, now, GRANT, "narrow");
        append(&journal, &stem, now, "bbbbbbbbbbbbbbbb", "wide");
        append(&journal, &stem, now, GRANT, "narrow again");
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        assert_eq!(users(&recalled), ["narrow again"]);
    }

    #[test]
    fn an_exchange_exactly_at_the_horizon_is_recalled_and_one_older_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), 1 << 20).expect("open");
        let stem = key().journal_stem();
        let now = UNIX_EPOCH + Duration::from_secs(10_000_000);
        let horizon = now - window().forget_after;
        append(
            &journal,
            &stem,
            horizon - Duration::from_millis(1),
            GRANT,
            "too old",
        );
        append(&journal, &stem, horizon, GRANT, "just in");
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        assert_eq!(users(&recalled), ["just in"]);
    }

    #[test]
    fn recalled_assets_keep_their_numbers_and_the_counter_moves_past_every_journaled_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), 1 << 20).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let inventory = photos(4, whatsapp);
        append_with(&journal, &stem, now, GRANT, "four photos", &inventory);
        append_with(&journal, &stem, now, GRANT, "what about them", &inventory);
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        let ids = recalled
            .assets
            .iter()
            .map(|asset| asset.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, [1, 2, 3, 4], "each asset is journaled once");
        assert_eq!(recalled.next_asset_id, 5);
        assert!(
            recalled
                .assets
                .iter()
                .all(|asset| matches!(asset.asset.source, Some(AssetSourceRef::WhatsApp { .. })))
        );
    }

    #[test]
    fn a_generated_asset_is_not_journaled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), 1 << 20).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let inventory = photos(1, |_| AssetSourceRef::Generated {
            capability: "image.generate".to_owned(),
            invocation: "one".to_owned(),
        });
        append_with(&journal, &stem, now, GRANT, "draw", &inventory);
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        assert!(recalled.assets.is_empty());
        assert_eq!(recalled.next_asset_id, 2);
    }

    #[test]
    fn whatsapp_media_past_its_lifetime_is_recalled_without_a_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), 1 << 20).expect("open");
        let stem = key().journal_stem();
        let now = UNIX_EPOCH + Duration::from_secs(100 * 24 * 60 * 60);
        let long = MemoryWindow {
            forget_after: Duration::from_secs(30 * 24 * 60 * 60),
            ..window()
        };
        let sent = now - WHATSAPP_MEDIA_LIFETIME - Duration::from_millis(1);
        let turn = ConversationTurn::completed("photo", "nice");
        let inventory = photos(1, whatsapp);
        journal
            .append(
                &stem,
                &Entry {
                    at: sent,
                    grant: GRANT,
                    turn: &turn,
                    inventory: &inventory,
                },
                long,
            )
            .expect("append");
        let recalled = journal.recall(&stem, GRANT, long, now).expect("recall");
        assert_eq!(recalled.assets.len(), 1);
        assert!(recalled.assets[0].asset.source.is_none());
    }

    #[test]
    fn a_file_past_its_threshold_compacts_to_the_window_and_carries_dropped_assets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), 1 << 20).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        append_with(&journal, &stem, now, GRANT, "photo", &photos(1, whatsapp));
        let filler = "x".repeat(200);
        for _ in 0..400 {
            append(&journal, &stem, now, GRANT, &filler);
        }
        let bytes = fs::metadata(journal.path(&stem)).expect("file").len();
        assert!(bytes <= compaction_threshold(window()), "{bytes} bytes");
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        assert_eq!(recalled.history.len(), 2, "two 400-byte turns fit 1 KiB");
        assert_eq!(recalled.assets.len(), 1, "the photo survives compaction");
    }

    #[test]
    fn the_least_recently_touched_file_is_evicted_at_the_byte_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), 400).expect("open");
        let now = SystemTime::now();
        let stems = ["a", "b", "c"].map(|conversation| {
            ConversationKey::shared(&"reviewer".parse().expect("agent"), "slack", conversation)
                .journal_stem()
        });
        for stem in &stems {
            append(&journal, stem, now, GRANT, &"y".repeat(100));
        }
        assert!(!journal.path(&stems[0]).exists(), "the oldest file is gone");
        assert!(journal.path(&stems[2]).exists(), "the newest file stays");
    }

    #[test]
    fn a_corrupt_file_is_refused_once_and_then_starts_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), 1 << 20).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        append(&journal, &stem, now, GRANT, "one");
        fs::write(journal.path(&stem), b"{\"not\": \"a line\"}\n").expect("corrupt");
        assert!(matches!(
            journal.recall(&stem, GRANT, window(), now),
            Err(JournalError::Corrupt)
        ));
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("reset");
        assert!(recalled.history.is_empty());
    }

    #[test]
    fn the_directory_and_its_files_are_private_to_the_daemon() {
        let parent = tempfile::tempdir().expect("tempdir");
        let dir = parent.path().join("journal");
        let journal = Journal::open(&dir, 1 << 20).expect("open");
        let stem = key().journal_stem();
        append(&journal, &stem, SystemTime::now(), GRANT, "one");
        let mode = |path: &Path| fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&journal.path(&stem)), 0o600);
        assert!(stem.chars().all(|character| character.is_ascii_hexdigit()));
    }
}
