use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, BufReader, Write},
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
    asset::{AssetRef, AssetSourceRef, PendingAsset, RecalledAsset},
    config::MemoryWindow,
};

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
    at_ms: u64,
    grant: String,
    user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answer: Option<String>,
    // The conversation's whole inventory when the line was written; recall reads only the newest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assets: Vec<LineAsset>,
    next_asset_id: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LineAsset {
    id: u64,
    at_ms: u64,
    name: String,
    mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    source: AssetSourceRef,
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
    pub next_asset_id: u64,
}

// Every method does blocking file I/O under `lock`; callers run it on a blocking thread, and the
// one lock is what orders appends, compaction and expiry for the whole directory.
pub(crate) struct Journal {
    dir: PathBuf,
    retention: Duration,
    lock: Mutex<()>,
}

impl Journal {
    pub fn open(dir: &Path, retention: Duration) -> Result<Self, JournalError> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let journal = Self {
            dir: dir.to_path_buf(),
            retention,
            lock: Mutex::new(()),
        };
        journal.expire(SystemTime::now())?;
        Ok(journal)
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    pub fn recall(
        &self,
        stem: &str,
        grant: &str,
        window: MemoryWindow,
        now: SystemTime,
    ) -> Result<Recalled, JournalError> {
        let _lock = self.lock.lock().expect("journal lock");
        self.expire(now)?;
        let mut lines = match read_lines(&self.path(stem)) {
            Ok(lines) => lines,
            Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            }) => Vec::new(),
            Err(JournalError::Corrupt) => {
                remove(&self.path(stem));
                return Err(JournalError::Corrupt);
            }
            Err(error) => return Err(error),
        };

        let now_ms = millis(now);
        let horizon_ms = now_ms.saturating_sub(duration_millis(window.forget_after));
        let media_horizon_ms = now_ms.saturating_sub(duration_millis(WHATSAPP_MEDIA_LIFETIME));
        // Only the trailing run under this grant is recalled, so a narrowed grant never replays
        // output produced under a wider one.
        let current = lines
            .iter()
            .rev()
            .take_while(|line| line.grant == grant && line.at_ms >= horizon_ms)
            .count();
        let mut kept = lines.split_off(lines.len() - current);
        let (assets, next_asset_id) = kept.last_mut().map_or((Vec::new(), 1), |last| {
            let assets = std::mem::take(&mut last.assets)
                .into_iter()
                .filter(|asset| asset.at_ms >= horizon_ms)
                .map(|asset| RecalledAsset {
                    id: asset.id,
                    arrived: UNIX_EPOCH + Duration::from_millis(asset.at_ms),
                    asset: PendingAsset {
                        name: asset.name,
                        mime: asset.mime,
                        size: asset.size,
                        source: match asset.source {
                            AssetSourceRef::WhatsApp { .. } if asset.at_ms < media_horizon_ms => {
                                None
                            }
                            source => Some(source),
                        },
                    },
                })
                .collect();
            (assets, last.next_asset_id)
        });
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
            next_asset_id,
        })
    }

    pub fn append(
        &self,
        stem: &str,
        entry: &Entry<'_>,
        window: MemoryWindow,
    ) -> Result<(), JournalError> {
        let _lock = self.lock.lock().expect("journal lock");
        let path = self.path(stem);
        let line = Line {
            at_ms: millis(entry.at),
            grant: entry.grant.to_owned(),
            user: entry.turn.user().to_owned(),
            answer: entry.turn.answer().map(str::to_owned),
            assets: entry
                .inventory
                .iter()
                .filter_map(|asset| {
                    // Generated outputs live in the broker's spool and do not outlive their session.
                    let source = asset
                        .source
                        .as_ref()
                        .filter(|source| !matches!(source, AssetSourceRef::Generated { .. }))?;
                    Some(LineAsset {
                        id: asset.id,
                        at_ms: millis(asset.arrived),
                        name: asset.name.clone(),
                        mime: asset.mime.clone(),
                        size: asset.size,
                        source: source.clone(),
                    })
                })
                .collect(),
            next_asset_id: entry.next_asset_id,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&encode_line(&line)?)?;
        let bytes = file.metadata()?.len();
        drop(file);
        if bytes > compaction_threshold(window) {
            match compact(&path, window) {
                Err(JournalError::Corrupt) => {
                    remove(&path);
                    return Err(JournalError::Corrupt);
                }
                result => result?,
            }
        }
        Ok(())
    }

    fn path(&self, stem: &str) -> PathBuf {
        self.dir.join(format!("{stem}.{EXTENSION}"))
    }

    // No route recalls a line older than the longest `forgetAfter`, so a file untouched for that
    // long holds nothing recallable.
    fn expire(&self, now: SystemTime) -> io::Result<()> {
        let Some(cutoff) = now.checked_sub(self.retention) else {
            return Ok(());
        };
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == EXTENSION)
                && entry.metadata()?.modified()? < cutoff
            {
                remove(&path);
                tracing::info!(event = "gateway_journal_expired");
            }
        }
        Ok(())
    }
}

fn remove(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(event = "gateway_journal_remove_failed", kind = %error.kind());
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

fn encode_line(line: &Line) -> Result<Vec<u8>, JournalError> {
    let mut encoded = serde_json::to_vec(line).map_err(io::Error::from)?;
    encoded.push(b'\n');
    Ok(encoded)
}

fn millis(at: SystemTime) -> u64 {
    duration_millis(at.duration_since(UNIX_EPOCH).unwrap_or_default())
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn read_lines(path: &Path) -> Result<Vec<Line>, JournalError> {
    let file = BufReader::new(File::open(path)?);
    serde_json::Deserializer::from_reader(file)
        .into_iter()
        .collect::<Result<_, _>>()
        .map_err(|_malformed| JournalError::Corrupt)
}

// Keeps the newest lines the window could replay; only the newest line's asset snapshot is ever
// recalled, so the older ones are dropped.
fn compact(path: &Path, window: MemoryWindow) -> Result<(), JournalError> {
    let mut lines = read_lines(path)?;
    let mut kept = 0;
    let mut bytes = 0usize;
    for line in lines.iter().rev() {
        bytes = bytes
            .saturating_add(line.user.len())
            .saturating_add(line.answer.as_ref().map_or(0, String::len));
        if kept > 0 && (kept >= window.limits.max_turns || bytes > window.limits.max_bytes) {
            break;
        }
        kept += 1;
    }
    let mut lines = lines.split_off(lines.len() - kept);
    if let Some((_, older)) = lines.split_last_mut() {
        for line in older {
            line.assets.clear();
        }
    }
    let temporary = path.with_extension("compact");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    for line in &lines {
        file.write_all(&encode_line(line)?)?;
    }
    drop(file);
    fs::rename(&temporary, path)?;
    Ok(())
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
    const RETENTION: Duration = Duration::from_secs(3600);

    fn window() -> MemoryWindow {
        MemoryWindow {
            scope: MemoryScope::PrivateConversation,
            idle_timeout: Duration::from_secs(300),
            limits: HistoryLimits {
                max_turns: 4,
                max_bytes: 1024,
            },
            recall: RecallSource::Journal,
            forget_after: RETENTION,
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
        append_turn(
            journal,
            stem,
            at,
            grant,
            &ConversationTurn::completed(user, format!("re: {user}")),
            inventory,
            window(),
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn append_turn(
        journal: &Journal,
        stem: &str,
        at: SystemTime,
        grant: &str,
        turn: &ConversationTurn,
        inventory: &[AssetRef],
        window: MemoryWindow,
    ) {
        let next_asset_id = inventory
            .iter()
            .map(|asset| asset.id + 1)
            .max()
            .unwrap_or(1);
        journal
            .append(
                stem,
                &Entry {
                    at,
                    grant,
                    turn,
                    inventory,
                    next_asset_id,
                },
                window,
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
            let journal = Journal::open(dir.path(), RETENTION).expect("open");
            append(&journal, &stem, now, GRANT, "one");
            append(&journal, &stem, now, GRANT, "two");
        }
        let journal = Journal::open(dir.path(), RETENTION).expect("reopen");
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        assert_eq!(users(&recalled), ["one", "two"]);
        assert_eq!(recalled.history.turns()[1].answer(), Some("re: two"));
    }

    #[test]
    fn an_escaped_answer_survives_compaction_and_reopen_with_earlier_photos() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let mut window = window();
        window.limits.max_bytes = 100_000;
        let answer = "\u{1}".repeat(90_000);
        let inventory = photos(1, whatsapp);
        {
            let journal = Journal::open(dir.path(), RETENTION).expect("open");
            let photo = ConversationTurn::completed("photo", "seen photo");
            append_turn(&journal, &stem, now, GRANT, &photo, &inventory, window);
            let escaped = ConversationTurn::completed("question", answer.as_str());
            append_turn(&journal, &stem, now, GRANT, &escaped, &inventory, window);
        }
        let journal = Journal::open(dir.path(), RETENTION).expect("reopen");
        let recalled = journal.recall(&stem, GRANT, window, now).expect("recall");
        assert_eq!(users(&recalled), ["photo", "question"]);
        assert_eq!(recalled.history.turns()[1].answer(), Some(answer.as_str()));
        assert_eq!(recalled.assets.len(), 1);
        assert_eq!(recalled.assets[0].id, inventory[0].id);
        assert!(
            matches!(recalled.assets[0].asset.source.as_ref(), Some(AssetSourceRef::WhatsApp { media_id, .. }) if media_id == "media-0")
        );
        assert_eq!(recalled.next_asset_id, 2);
    }

    #[test]
    fn a_conversation_with_no_file_recalls_an_empty_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION).expect("open");
        let recalled = journal
            .recall(&key().journal_stem(), GRANT, window(), SystemTime::now())
            .expect("recall");
        assert!(recalled.history.is_empty());
        assert_eq!(recalled.next_asset_id, 1);
    }

    #[test]
    fn only_the_trailing_run_under_the_current_grant_is_recalled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION).expect("open");
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
        let journal = Journal::open(dir.path(), RETENTION).expect("open");
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
    fn recall_restores_the_newest_snapshot_under_its_numbers_and_counter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let inventory = photos(4, whatsapp);
        append_with(&journal, &stem, now, GRANT, "two photos", &inventory[..2]);
        append_with(&journal, &stem, now, GRANT, "four photos", &inventory);
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        let ids = recalled
            .assets
            .iter()
            .map(|asset| asset.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, [1, 2, 3, 4]);
        assert_eq!(recalled.next_asset_id, 5);
    }

    #[test]
    fn a_generated_asset_is_not_journaled_but_keeps_its_number() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION).expect("open");
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
        let journal = Journal::open(dir.path(), RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = UNIX_EPOCH + Duration::from_secs(100 * 24 * 60 * 60);
        let long = MemoryWindow {
            forget_after: Duration::from_secs(30 * 24 * 60 * 60),
            ..window()
        };
        let mut inventory = photos(1, whatsapp);
        inventory[0].arrived = now - WHATSAPP_MEDIA_LIFETIME - Duration::from_millis(1);
        let turn = ConversationTurn::completed("still about that photo", "yes");
        append_turn(&journal, &stem, now, GRANT, &turn, &inventory, long);
        let recalled = journal.recall(&stem, GRANT, long, now).expect("recall");
        assert_eq!(recalled.assets.len(), 1);
        assert!(recalled.assets[0].asset.source.is_none());
    }

    #[test]
    fn an_asset_older_than_the_horizon_is_not_recalled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let mut inventory = photos(2, whatsapp);
        inventory[0].arrived = now - RETENTION - Duration::from_millis(1);
        append_with(&journal, &stem, now, GRANT, "still talking", &inventory);
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        let ids = recalled
            .assets
            .iter()
            .map(|asset| asset.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, [inventory[1].id]);
        assert_eq!(recalled.next_asset_id, 3);
    }

    #[test]
    fn a_file_past_its_threshold_compacts_to_the_window_and_keeps_the_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let inventory = photos(1, whatsapp);
        let filler = "x".repeat(200);
        for _ in 0..400 {
            append_with(&journal, &stem, now, GRANT, &filler, &inventory);
        }
        let bytes = fs::metadata(journal.path(&stem)).expect("file").len();
        assert!(bytes <= compaction_threshold(window()), "{bytes} bytes");
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        assert_eq!(recalled.history.len(), 2, "two 400-byte turns fit 1 KiB");
        assert_eq!(recalled.assets.len(), 1);
    }

    #[test]
    fn a_file_untouched_past_the_retention_is_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION).expect("open");
        let now = SystemTime::now();
        let stems = ["stale", "fresh"].map(|conversation| {
            ConversationKey::shared(&"reviewer".parse().expect("agent"), "slack", conversation)
                .journal_stem()
        });
        for stem in &stems {
            append(&journal, stem, now, GRANT, "hello");
        }
        File::options()
            .write(true)
            .open(journal.path(&stems[0]))
            .expect("open stale")
            .set_modified(now - RETENTION - Duration::from_secs(1))
            .expect("age stale");
        journal
            .recall(&stems[1], GRANT, window(), now)
            .expect("recall");
        assert!(!journal.path(&stems[0]).exists());
        assert!(journal.path(&stems[1]).exists());
    }

    #[test]
    fn a_file_from_an_older_format_is_refused_once_and_then_starts_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let old = format!(
            "{{\"v\":1,\"atMs\":{},\"grant\":\"{GRANT}\",\"user\":\"one\",\"lastAssetId\":3}}\n",
            millis(now)
        );
        fs::write(journal.path(&stem), old).expect("old format");
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
        let journal = Journal::open(&dir, RETENTION).expect("open");
        let stem = key().journal_stem();
        append(&journal, &stem, SystemTime::now(), GRANT, "one");
        let mode = |path: &Path| fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&journal.path(&stem)), 0o600);
        assert!(stem.chars().all(|character| character.is_ascii_hexdigit()));
    }
}
