use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, BufReader, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dekopon_agent::prompt::{
    ConversationTurn, Exchange, History, Pin, ReadRef, Representation, Selection,
};
use dekopon_core::AgentId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    asset::{AssetRef, AssetSourceRef, PendingAsset, RecalledAsset},
    config::MemoryWindow,
    conversation::SealedConversation,
};

const EXTENSION: &str = "jsonl";
const SEAL_EXTENSION: &str = "sealed";
const FOLLOW_EXTENSION: &str = "followed";
const MAX_FOLLOW_BYTES: u64 = 4096;
const MAX_SEAL_BYTES: u64 = 4096;
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
    version: u8,
    at_ms: u64,
    grant: String,
    record_id: u64,
    next_record_id: u64,
    source_eligibility: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    answer: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    representations: Vec<Representation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exchanges: Vec<Exchange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pins: Vec<Pin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    reads: Vec<ReadRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    selections: Vec<Selection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    availability: Vec<String>,
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

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Seal {
    at_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct FollowMarker {
    pub at_ms: u64,
    pub until_ms: u64,
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
    max_age: Duration,
    pub(crate) agent_ages: HashMap<String, Duration>,
    pub(crate) agent_bytes: HashMap<String, u64>,
    pub(crate) max_bytes_per_agent: u64,
    retention: Duration,
    seal_retention: Duration,
    lock: Mutex<()>,
}

impl Journal {
    #[cfg(test)]
    pub fn open(
        dir: &Path,
        retention: Duration,
        seal_retention: Duration,
    ) -> Result<Self, JournalError> {
        Self::open_with_age(dir, retention, retention, seal_retention)
    }

    pub fn open_with_age(
        dir: &Path,
        max_age: Duration,
        retention: Duration,
        seal_retention: Duration,
    ) -> Result<Self, JournalError> {
        Self::open_at_with_age(dir, max_age, retention, seal_retention, SystemTime::now())
    }

    #[cfg(test)]
    pub(crate) fn open_at(
        dir: &Path,
        retention: Duration,
        seal_retention: Duration,
        now: SystemTime,
    ) -> Result<Self, JournalError> {
        Self::open_at_with_age(dir, retention, retention, seal_retention, now)
    }

    fn open_at_with_age(
        dir: &Path,
        max_age: Duration,
        retention: Duration,
        seal_retention: Duration,
        now: SystemTime,
    ) -> Result<Self, JournalError> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let journal = Self {
            dir: dir.to_path_buf(),
            max_age,
            agent_ages: HashMap::new(),
            agent_bytes: HashMap::new(),
            max_bytes_per_agent: 64 * 1024 * 1024,
            retention,
            seal_retention,
            lock: Mutex::new(()),
        };
        journal.reset_invalid_files()?;
        journal.expire(now)?;
        Ok(journal)
    }

    pub fn seal(&self, stem: &str, at: SystemTime) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(self.seal_path(stem))?;
        serde_json::to_writer(&mut file, &Seal { at_ms: millis(at) }).map_err(io::Error::from)?;
        Ok(())
    }

    pub fn sealed(
        &self,
        stem: &str,
        forget_after: Duration,
    ) -> Result<Option<SealedConversation>, JournalError> {
        let _lock = self.lock.lock();
        self.read_seal(stem, forget_after)
    }

    fn follow_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.{FOLLOW_EXTENSION}"))
    }

    pub(crate) fn write_follow(&self, key: &str, marker: FollowMarker) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(self.follow_path(key))?;
        serde_json::to_writer(&mut file, &marker).map_err(io::Error::from)?;
        file.sync_all()?;
        Ok(())
    }

    pub(crate) fn remove_follow(&self, key: &str) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        match fs::remove_file(self.follow_path(key)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn load_follow(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, FollowMarker)>, JournalError> {
        let _lock = self.lock.lock();
        let mut markers = BTreeMap::new();
        let mut overflow = 0usize;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path
                .extension()
                .is_none_or(|extension| extension != FOLLOW_EXTENSION)
            {
                continue;
            }
            let Some(key) = path.file_stem().and_then(|name| name.to_str()) else {
                remove(&path);
                tracing::warn!(
                    event = "gateway_follow",
                    operation = "refused",
                    reason = "corrupt"
                );
                continue;
            };
            if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                remove(&path);
                tracing::warn!(
                    event = "gateway_follow",
                    operation = "refused",
                    reason = "corrupt"
                );
                continue;
            }
            match read_follow_marker(&path)? {
                Some(marker) => {
                    markers.insert((marker.until_ms, key.to_owned()), marker);
                    if markers.len() > limit {
                        markers.pop_first();
                        overflow += 1;
                    }
                }
                None => {
                    remove(&path);
                    tracing::warn!(
                        event = "gateway_follow",
                        operation = "refused",
                        reason = "corrupt"
                    );
                }
            }
        }
        if overflow != 0 {
            let retained: HashSet<_> = markers.keys().map(|(_, key)| key.as_str()).collect();
            for entry in fs::read_dir(&self.dir)? {
                let entry = entry?;
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == FOLLOW_EXTENSION)
                    && let Some(key) = path.file_stem().and_then(|name| name.to_str())
                    && key.len() == 64
                    && key.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && !retained.contains(key)
                {
                    remove(&path);
                }
            }
            tracing::warn!(
                event = "gateway_follow",
                operation = "refused",
                reason = "over-capacity",
                count = overflow
            );
        }
        Ok(markers
            .into_iter()
            .map(|((_, key), marker)| (key, marker))
            .collect())
    }

    fn seal_path(&self, stem: &str) -> PathBuf {
        let name = Path::new(stem).file_name().unwrap_or_default();
        self.dir.join(name).with_extension(SEAL_EXTENSION)
    }

    fn read_seal(
        &self,
        stem: &str,
        forget_after: Duration,
    ) -> Result<Option<SealedConversation>, JournalError> {
        let path = self.seal_path(stem);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let modified = file.metadata()?.modified()?;
        if SystemTime::now()
            .checked_sub(forget_after)
            .is_some_and(|cutoff| modified < cutoff)
        {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        file.take(MAX_SEAL_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SEAL_BYTES {
            remove(&path);
            return Err(JournalError::Corrupt);
        }
        match serde_json::from_slice::<Seal>(&bytes) {
            Ok(seal) => match UNIX_EPOCH.checked_add(Duration::from_millis(seal.at_ms)) {
                Some(at) => Ok(Some(SealedConversation { at })),
                None => {
                    remove(&path);
                    Err(JournalError::Corrupt)
                }
            },
            Err(_) => {
                remove(&path);
                Err(JournalError::Corrupt)
            }
        }
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
        let _lock = self.lock.lock();
        self.expire(now)?;
        let seal_ms = self
            .read_seal(stem, window.forget_after)?
            .map(|seal| millis(seal.at));
        let now_ms = millis(now);
        let horizon_ms = now_ms.saturating_sub(duration_millis(window.forget_after));
        let age_horizon_ms = now_ms.saturating_sub(duration_millis(self.age_for(stem)));
        let media_horizon_ms = now_ms.saturating_sub(duration_millis(WHATSAPP_MEDIA_LIFETIME));
        let (history, mut last) = match read_window(
            &self.path(stem),
            grant,
            window,
            seal_ms,
            horizon_ms,
            age_horizon_ms,
        ) {
            Ok(recalled) => recalled,
            Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            }) => (History::new(window.limits), None),
            Err(JournalError::Corrupt) => {
                remove(&self.path(stem));
                return Err(JournalError::Corrupt);
            }
            Err(error) => return Err(error),
        };
        let (assets, next_asset_id) = last.as_mut().map_or((Vec::new(), 1), |last| {
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
        let _lock = self.lock.lock();
        let path = self.path(stem);
        let next_record_id = match read_lines(&path) {
            Ok(lines) => lines.last().map_or(1, |last| last.next_record_id),
            Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            }) => 1,
            Err(error) => return Err(error),
        };
        let line = Line {
            version: 2,
            at_ms: millis(entry.at),
            record_id: next_record_id,
            next_record_id: next_record_id.checked_add(1).ok_or(JournalError::Corrupt)?,
            source_eligibility: true,
            representations: Vec::new(),
            exchanges: Vec::new(),
            pins: Vec::new(),
            reads: Vec::new(),
            selections: Vec::new(),
            availability: vec!["original".to_owned()],
            grant: entry.grant.to_owned(),
            user: Some(entry.turn.user().to_owned()),
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
        if let Some(parent) = path.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        let encoded = encode_line(&line)?;
        file.write_all(&encoded)?;
        let bytes = file.metadata()?.len();
        drop(file);
        if bytes > compaction_threshold(window) {
            match compact(&path, self.age_for(stem), entry.at) {
                Err(JournalError::Corrupt) => {
                    remove(&path);
                    return Err(JournalError::Corrupt);
                }
                result => result?,
            }
        }
        if let Some((agent, _)) = stem.split_once('/')
            && self.enforce_agent_cap(agent, Some((&path, line.record_id)))?
        {
            tracing::warn!(
                event = "gateway_memory_retention_failed",
                reason = "agent-byte-cap",
                encoded_bytes = encoded.len()
            );
        }
        Ok(())
    }

    pub(crate) fn enforce_caps(&self) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if is_agent_directory(&path)
                && let Some(agent) = path.file_name().and_then(|name| name.to_str())
            {
                self.enforce_agent_cap(agent, None)?;
            }
        }
        Ok(())
    }

    fn enforce_agent_cap(
        &self,
        agent: &str,
        appended: Option<(&Path, u64)>,
    ) -> Result<bool, JournalError> {
        let limit = self
            .agent_bytes
            .get(agent)
            .copied()
            .unwrap_or(self.max_bytes_per_agent);
        let directory = self.dir.join(agent);
        if !directory.exists() {
            return Ok(false);
        }
        let mut paths = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if is_journal_file(&path) {
                let size = fs::metadata(&path)?.len();
                total = total.saturating_add(size);
                paths.push((path, size));
            }
        }
        if total <= limit {
            return Ok(false);
        }
        let mut files = paths
            .into_iter()
            .map(|(path, size)| {
                let lines = read_lines(&path)?;
                Ok((path, size, lines, 0usize))
            })
            .collect::<Result<Vec<_>, JournalError>>()?;
        let mut lost_append = false;
        while total > limit {
            let oldest = files
                .iter()
                .enumerate()
                .filter_map(|(index, (path, _, lines, removed))| {
                    lines.get(*removed).map(|line| (line.at_ms, path, index))
                })
                .min_by_key(|(at, path, _)| (*at, *path));
            let Some((_, _, index)) = oldest else {
                break;
            };
            let (path, size, lines, removed) = &mut files[index];
            lost_append |= appended
                .is_some_and(|(new_path, id)| new_path == path && lines[*removed].record_id == id);
            let dropped = encode_line(&lines[*removed])?.len() as u64;
            *removed += 1;
            *size = size.saturating_sub(dropped);
            total = total.saturating_sub(dropped);
        }
        for (path, _, lines, removed) in files {
            if removed == 0 {
                continue;
            }
            if removed == lines.len() {
                fs::remove_file(path)?;
            } else {
                rewrite(&path, &lines[removed..])?;
            }
        }
        Ok(lost_append)
    }

    fn path(&self, stem: &str) -> PathBuf {
        self.dir.join(format!("{stem}.{EXTENSION}"))
    }

    fn age_for(&self, stem: &str) -> Duration {
        stem.split_once('/')
            .and_then(|(agent, _)| self.agent_ages.get(agent))
            .copied()
            .unwrap_or(self.max_age)
    }

    fn reset_invalid_files(&self) -> Result<(), JournalError> {
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if is_agent_directory(&path) {
                for file in fs::read_dir(path)? {
                    let file = file?;
                    if is_journal_file(&file.path())
                        && matches!(read_lines(&file.path()), Err(JournalError::Corrupt))
                    {
                        remove(&file.path());
                        tracing::warn!(event = "gateway_journal_reset", reason = "corrupt");
                    }
                }
            } else if is_journal_file(&path) && !path.with_extension(FOLLOW_EXTENSION).exists() {
                remove(&path);
                tracing::warn!(event = "gateway_journal_reset", reason = "legacy");
            }
        }
        Ok(())
    }

    // Follow markers expire by their lifecycle deadline, not the journal window's file mtime.
    fn expire(&self, now: SystemTime) -> Result<(), JournalError> {
        let cutoff = now.checked_sub(self.retention);
        let seal_cutoff = now.checked_sub(self.seal_retention);
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if is_agent_directory(&path) {
                for file in fs::read_dir(&path)? {
                    let file = file?;
                    if is_journal_file(&file.path()) {
                        let file_path = file.path();
                        if cutoff.is_some_and(|limit| {
                            file.metadata()
                                .and_then(|m| m.modified())
                                .is_ok_and(|modified| modified < limit)
                        }) {
                            remove(&file_path);
                            tracing::info!(event = "gateway_journal_expired");
                        } else {
                            let age = self.age_for(&format!(
                                "{}/{}",
                                path.file_name().unwrap_or_default().to_string_lossy(),
                                file_path.file_stem().unwrap_or_default().to_string_lossy()
                            ));
                            let horizon = millis(now).saturating_sub(duration_millis(age));
                            match read_lines(&file_path) {
                                Ok(lines) if lines.iter().any(|line| line.at_ms < horizon) => {
                                    if lines.iter().all(|line| line.at_ms < horizon) {
                                        remove(&file_path);
                                    } else {
                                        compact(&file_path, age, now)?;
                                    }
                                    tracing::info!(event = "gateway_journal_expired");
                                }
                                Err(JournalError::Corrupt) => {
                                    remove(&file_path);
                                    tracing::warn!(
                                        event = "gateway_journal_reset",
                                        reason = "corrupt"
                                    );
                                }
                                Err(error) => return Err(error),
                                Ok(_) => {}
                            }
                        }
                    }
                }
                continue;
            }
            let stale = match path.extension() {
                Some(extension) if extension == SEAL_EXTENSION => {
                    let modified = entry.metadata()?.modified()?;
                    seal_cutoff.is_some_and(|limit| modified < limit)
                }
                Some(extension) if extension == FOLLOW_EXTENSION => {
                    read_follow_marker(&path)?.is_some_and(|marker| marker.until_ms <= millis(now))
                }
                _ => false,
            };
            if stale {
                remove(&path);
                tracing::info!(event = "gateway_journal_expired");
            }
        }
        Ok(())
    }
}

fn is_agent_directory(path: &Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.parse::<AgentId>().is_ok())
}

fn is_journal_file(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension == EXTENSION)
        && path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|stem| {
                stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

fn read_follow_marker(path: &Path) -> io::Result<Option<FollowMarker>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_FOLLOW_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FOLLOW_BYTES {
        return Ok(None);
    }
    Ok(serde_json::from_slice::<FollowMarker>(&bytes)
        .ok()
        .filter(|marker| marker.at_ms < marker.until_ms))
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

fn read_window(
    path: &Path,
    grant: &str,
    window: MemoryWindow,
    seal_ms: Option<u64>,
    horizon_ms: u64,
    age_horizon_ms: u64,
) -> Result<(History, Option<Line>), JournalError> {
    let mut history = History::new(window.limits);
    let mut last = None;
    let mut previous = None;
    for item in
        serde_json::Deserializer::from_reader(BufReader::new(File::open(path)?)).into_iter::<Line>()
    {
        let mut line = item.map_err(|_malformed| JournalError::Corrupt)?;
        if line.version != 2
            || previous.is_some_and(|id| id != line.record_id)
            || line.next_record_id != line.record_id.saturating_add(1)
            || (!line.source_eligibility && (line.user.is_some() || line.answer.is_some()))
        {
            return Err(JournalError::Corrupt);
        }
        previous = Some(line.next_record_id);
        if line.grant != grant
            || line.at_ms < horizon_ms
            || line.at_ms < age_horizon_ms
            || seal_ms.is_some_and(|seal| line.at_ms <= seal)
        {
            history = History::new(window.limits);
            last = None;
            continue;
        }
        if let Some(user) = line.user.take() {
            let turn = match line.answer.take() {
                Some(answer) => ConversationTurn::completed(user, answer),
                None => ConversationTurn::unanswered(user),
            };
            history.record(turn);
        }
        last = Some(line);
    }
    Ok((history, last))
}

fn read_lines(path: &Path) -> Result<Vec<Line>, JournalError> {
    let file = BufReader::new(File::open(path)?);
    let lines: Vec<Line> = serde_json::Deserializer::from_reader(file)
        .into_iter()
        .collect::<Result<_, _>>()
        .map_err(|_malformed| JournalError::Corrupt)?;
    if lines.iter().any(|line| line.version != 2)
        || lines
            .windows(2)
            .any(|pair| pair[0].next_record_id != pair[1].record_id)
        || lines.iter().any(|line| {
            line.next_record_id != line.record_id.saturating_add(1)
                || (!line.source_eligibility && (line.user.is_some() || line.answer.is_some()))
        })
    {
        return Err(JournalError::Corrupt);
    }
    Ok(lines)
}

fn compact(path: &Path, max_age: Duration, now: SystemTime) -> Result<(), JournalError> {
    let horizon = millis(now).saturating_sub(duration_millis(max_age));
    let lines = read_lines(path)?;
    rewrite(
        path,
        &lines
            .into_iter()
            .filter(|line| line.at_ms >= horizon)
            .collect::<Vec<_>>(),
    )
}

fn rewrite(path: &Path, lines: &[Line]) -> Result<(), JournalError> {
    let temporary = path.with_extension("compact");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    for line in lines {
        file.write_all(&encode_line(line)?)?;
    }
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
impl Journal {
    pub(crate) fn hold_io_for_test(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.lock.lock()
    }
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
            let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
            append(&journal, &stem, now, GRANT, "one");
            append(&journal, &stem, now, GRANT, "two");
        }
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("reopen");
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
            let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
            let photo = ConversationTurn::completed("photo", "seen photo");
            append_turn(&journal, &stem, now, GRANT, &photo, &inventory, window);
            let escaped = ConversationTurn::completed("question", answer.as_str());
            append_turn(&journal, &stem, now, GRANT, &escaped, &inventory, window);
        }
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("reopen");
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
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let recalled = journal
            .recall(&key().journal_stem(), GRANT, window(), SystemTime::now())
            .expect("recall");
        assert!(recalled.history.is_empty());
        assert_eq!(recalled.next_asset_id, 1);
    }

    #[test]
    fn only_the_trailing_run_under_the_current_grant_is_recalled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
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
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
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
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
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
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
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
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
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
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
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
    fn a_file_past_its_threshold_keeps_eligible_turns_and_the_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let inventory = photos(1, whatsapp);
        let filler = "x".repeat(200);
        for _ in 0..400 {
            append_with(&journal, &stem, now, GRANT, &filler, &inventory);
        }
        let bytes = fs::metadata(journal.path(&stem)).expect("file").len();
        assert!(bytes > compaction_threshold(window()), "{bytes} bytes");
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        assert_eq!(recalled.history.len(), 2, "two 400-byte turns fit 1 KiB");
        assert_eq!(recalled.assets.len(), 1);
    }

    #[test]
    fn a_file_untouched_past_the_retention_is_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
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
    fn a_file_from_an_older_format_resets_during_sweep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let old = format!(
            "{{\"v\":1,\"atMs\":{},\"grant\":\"{GRANT}\",\"user\":\"one\",\"lastAssetId\":3}}\n",
            millis(now)
        );
        fs::create_dir_all(journal.path(&stem).parent().unwrap()).unwrap();
        fs::write(journal.path(&stem), old).expect("old format");
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("reset");
        assert!(recalled.history.is_empty());
        assert!(!journal.path(&stem).exists());
    }

    #[test]
    fn opening_resets_unversioned_root_files_without_touching_follow_markers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stem = key().journal_stem();
        let digest = stem.split_once('/').unwrap().1;
        let legacy = dir.path().join(format!("{digest}.jsonl"));
        fs::write(&legacy, b"{\"user\":\"old\"}\n").unwrap();
        let followed = dir.path().join(format!("{digest}.followed"));
        fs::write(&followed, b"marker").unwrap();
        Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        assert!(legacy.exists());
        assert!(followed.exists());
        fs::remove_file(&followed).unwrap();
        let (capture, _guard) = crate::tests::capture_spans();
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        assert_eq!(
            capture
                .events_where(
                    "dekopon_gatewayd",
                    &[("event", "gateway_journal_reset"), ("reason", "legacy")]
                )
                .len(),
            1
        );
        assert!(!legacy.exists());
        assert!(
            users(
                &journal
                    .recall(&stem, GRANT, window(), SystemTime::now())
                    .unwrap()
            )
            .is_empty()
        );
        drop(journal);
    }

    #[test]
    fn v2_ids_round_trip_and_an_unknown_version_resets_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        append(&journal, &stem, now, GRANT, "first");
        append(&journal, &stem, now, GRANT, "second");
        let path = journal.path(&stem);
        let lines = read_lines(&path).unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.version, line.record_id, line.next_record_id))
                .collect::<Vec<_>>(),
            [(2, 1, 2), (2, 2, 3)]
        );
        drop(journal);
        let reopened = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        assert_eq!(
            users(&reopened.recall(&stem, GRANT, window(), now).unwrap()),
            ["first", "second"]
        );
        drop(reopened);
        let mut lines = read_lines(&path).unwrap();
        lines[0].version = 3;
        let mut bytes = Vec::new();
        for line in &lines {
            bytes.extend(encode_line(line).unwrap());
        }
        fs::write(&path, bytes).unwrap();
        let (capture, _guard) = crate::tests::capture_spans();
        let reset = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        assert_eq!(
            capture
                .events_where(
                    "dekopon_gatewayd",
                    &[("event", "gateway_journal_reset"), ("reason", "corrupt")]
                )
                .len(),
            1
        );
        assert!(!path.exists());
        assert!(users(&reset.recall(&stem, GRANT, window(), now).unwrap()).is_empty());
    }

    #[test]
    fn compaction_retains_window_evicted_records_until_memory_age() {
        let dir = tempfile::tempdir().unwrap();
        let age = Duration::from_secs(2 * 3600);
        let journal = Journal::open_with_age(dir.path(), age, age, RETENTION).unwrap();
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let earlier = now - Duration::from_secs(1800);
        append(&journal, &stem, earlier, GRANT, "earlier");
        let filler = "x".repeat(400);
        for _ in 0..200 {
            append(&journal, &stem, now, GRANT, &filler);
        }
        let lines = read_lines(&journal.path(&stem)).unwrap();
        assert_eq!(lines[0].user.as_deref(), Some("earlier"));
        assert!(
            users(&journal.recall(&stem, GRANT, window(), now).unwrap())
                .iter()
                .all(|user| *user != "earlier")
        );
        compact(&journal.path(&stem), age, now + age).unwrap();
        assert!(
            read_lines(&journal.path(&stem))
                .unwrap()
                .iter()
                .all(|line| line.user.as_deref() != Some("earlier"))
        );
    }

    #[test]
    fn agent_directories_keep_independent_record_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        let now = SystemTime::now();
        let other =
            ConversationKey::shared(&"another".parse().unwrap(), "slack", "same").journal_stem();
        let first =
            ConversationKey::shared(&"reviewer".parse().unwrap(), "slack", "same").journal_stem();
        append(&journal, &first, now, GRANT, "first");
        append(&journal, &other, now, GRANT, "other");
        assert_eq!(read_lines(&journal.path(&first)).unwrap()[0].record_id, 1);
        assert_eq!(read_lines(&journal.path(&other)).unwrap()[0].record_id, 1);
        assert_ne!(journal.path(&first).parent(), journal.path(&other).parent());
    }

    #[test]
    fn encoded_agent_cap_evicts_oldest_whole_turn_across_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        let now = SystemTime::now();
        let first = key().journal_stem();
        let second =
            ConversationKey::shared(&"reviewer".parse().unwrap(), "slack", "second").journal_stem();
        let other =
            ConversationKey::shared(&"another".parse().unwrap(), "slack", "third").journal_stem();
        append(
            &journal,
            &first,
            now - Duration::from_secs(2),
            GRANT,
            "middle",
        );
        append(
            &journal,
            &second,
            now - Duration::from_secs(3),
            GRANT,
            "old",
        );
        append(&journal, &first, now - Duration::from_secs(1), GRANT, "new");
        append(&journal, &other, now, GRANT, "other agent");
        let old_size = fs::metadata(journal.path(&second)).unwrap().len();
        let total = fs::metadata(journal.path(&first)).unwrap().len()
            + fs::metadata(journal.path(&second)).unwrap().len();
        journal
            .agent_bytes
            .insert("reviewer".into(), total - old_size);
        journal.enforce_caps().unwrap();
        assert_eq!(
            users(&journal.recall(&first, GRANT, window(), now).unwrap()),
            ["middle", "new"]
        );
        assert!(!journal.path(&second).exists());
        assert_eq!(
            users(&journal.recall(&other, GRANT, window(), now).unwrap()),
            ["other agent"]
        );
        assert_eq!(read_lines(&journal.path(&first)).unwrap()[0].record_id, 1);
        append(&journal, &first, now, GRANT, "after cap");
        assert_eq!(
            read_lines(&journal.path(&first))
                .unwrap()
                .last()
                .unwrap()
                .record_id,
            3
        );
        let remaining = [first, second]
            .iter()
            .map(|stem| fs::metadata(journal.path(stem)).map_or(0, |meta| meta.len()))
            .sum::<u64>();
        assert!(remaining <= total - old_size);
        assert_eq!(
            users(&journal.recall(&other, GRANT, window(), now).unwrap()),
            ["other agent"]
        );
    }

    #[test]
    fn oversized_turn_emits_bounded_retention_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        journal.agent_bytes.insert("reviewer".into(), 1);
        let (capture, _guard) = crate::tests::capture_spans();
        append(
            &journal,
            &key().journal_stem(),
            SystemTime::now(),
            GRANT,
            "oversized",
        );
        let failures = capture.events_where(
            "dekopon_gatewayd",
            &[
                ("event", "gateway_memory_retention_failed"),
                ("reason", "agent-byte-cap"),
            ],
        );
        assert_eq!(failures.len(), 1);
        assert!(failures[0].field("encoded_bytes").is_some());
        assert!(failures[0].field("user").is_none());
        assert!(!journal.path(&key().journal_stem()).exists());
    }

    #[test]
    fn failed_cap_rewrite_keeps_the_previous_whole_turns() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        let stem = key().journal_stem();
        let now = SystemTime::now();
        append(&journal, &stem, now - Duration::from_secs(1), GRANT, "old");
        append(&journal, &stem, now, GRANT, "new");
        let path = journal.path(&stem);
        let before = fs::read(&path).unwrap();
        let first_size = encode_line(&read_lines(&path).unwrap()[0]).unwrap().len() as u64;
        journal
            .agent_bytes
            .insert("reviewer".into(), before.len() as u64 - first_size);
        fs::create_dir(path.with_extension("compact")).unwrap();
        assert!(matches!(
            journal.enforce_caps(),
            Err(JournalError::Io { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn agent_specific_age_excludes_old_records_without_expiring_a_longer_agents_file() {
        let dir = tempfile::tempdir().unwrap();
        let long = Duration::from_secs(7 * 86400);
        let short = Duration::from_secs(86400);
        let now = SystemTime::now();
        let mut journal = Journal::open_with_age(dir.path(), long, long, long).unwrap();
        journal.agent_ages.insert("reviewer".into(), short);
        let stem = key().journal_stem();
        append(
            &journal,
            &stem,
            now - 2 * short,
            GRANT,
            "expired for reviewer",
        );
        append(&journal, &stem, now, GRANT, "recent for reviewer");
        let path = journal.path(&stem);
        let mut wide = window();
        wide.forget_after = long;
        assert_eq!(
            users(&journal.recall(&stem, GRANT, wide, now).unwrap()),
            ["recent for reviewer"]
        );
        assert!(path.exists());
        assert_eq!(read_lines(&path).unwrap().len(), 1);
    }

    #[test]
    fn unavailable_original_does_not_reappear_in_v2_or_chat_recall() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        let stem = key().journal_stem();
        let now = SystemTime::now();
        append(&journal, &stem, now, GRANT, "source sentinel");
        let path = journal.path(&stem);
        let mut lines = read_lines(&path).unwrap();
        let record = &mut lines[0];
        record.user = None;
        record.answer = None;
        record.source_eligibility = false;
        record.availability.clear();
        record.representations.push(Representation {
            routine_id: "prefix".into(),
            version: "resolved-config-digest".into(),
            source_record_id: record.record_id,
            part: None,
            kind: "prefix".into(),
            captured_bytes: 15,
            text: Some("summary".into()),
        });
        fs::write(&path, encode_line(record).unwrap()).unwrap();
        let reopened = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(
            !bytes
                .windows(b"source sentinel".len())
                .any(|part| part == b"source sentinel")
        );
        let record = read_lines(&path).unwrap().pop().unwrap();
        assert_eq!(record.representations[0].text.as_deref(), Some("summary"));
        assert!(
            reopened
                .recall(&stem, GRANT, window(), now)
                .unwrap()
                .history
                .is_empty()
        );
    }

    #[test]
    fn failed_rewrite_preserves_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        let stem = key().journal_stem();
        let now = SystemTime::now();
        append(&journal, &stem, now, GRANT, "intact");
        let path = journal.path(&stem);
        let before = fs::read(&path).unwrap();
        fs::create_dir(path.with_extension("compact")).unwrap();
        assert!(matches!(
            compact(&path, RETENTION, now),
            Err(JournalError::Io { .. })
        ));
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn the_directory_and_its_files_are_private_to_the_daemon() {
        let parent = tempfile::tempdir().expect("tempdir");
        let dir = parent.path().join("journal");
        let journal = Journal::open(&dir, RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        append(&journal, &stem, SystemTime::now(), GRANT, "one");
        let mode = |path: &Path| fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&journal.path(&stem)), 0o600);
        assert!(stem.split_once('/').is_some_and(|(agent, digest)| {
            agent == "reviewer"
                && digest
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
        }));
    }
    #[test]
    fn a_seal_has_private_sibling_storage_and_leaves_the_transcript_unchanged() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        append(&journal, &stem, now, GRANT, "old turn");
        let before = fs::read(journal.path(&stem)).unwrap();
        journal.seal(&stem, now).unwrap();
        assert_eq!(fs::read(journal.path(&stem)).unwrap(), before);
        let path = journal.seal_path(&stem);
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({"atMs": millis(now)}));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_seal_filters_journal_lines_at_and_before_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        append(
            &journal,
            &stem,
            now - Duration::from_secs(1),
            GRANT,
            "before",
        );
        append(&journal, &stem, now, GRANT, "at");
        append(
            &journal,
            &stem,
            now + Duration::from_secs(1),
            GRANT,
            "after",
        );
        journal.seal(&stem, now).unwrap();
        let recalled = journal
            .recall(&stem, GRANT, window(), now + Duration::from_secs(2))
            .unwrap();
        assert_eq!(users(&recalled), ["after"]);
    }

    #[test]
    fn a_corrupt_seal_is_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let path = journal.seal_path(&stem);
        fs::write(&path, b"not a marker").unwrap();
        assert!(matches!(
            journal.sealed(&stem, RETENTION),
            Err(JournalError::Corrupt)
        ));
        assert!(!path.exists());
    }

    #[test]
    fn an_oversized_seal_is_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let path = journal.seal_path(&stem);
        let mut marker = br#"{"atMs":1}"#.to_vec();
        marker.resize((MAX_SEAL_BYTES + 1) as usize, b' ');
        fs::write(&path, marker).unwrap();
        assert!(matches!(
            journal.sealed(&stem, RETENTION),
            Err(JournalError::Corrupt)
        ));
        assert!(!path.exists());
    }

    #[test]
    fn reading_a_seal_does_not_expire_other_conversations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let old = journal.seal_path("other");
        fs::write(&old, br#"{"atMs":1}"#).unwrap();
        let now = SystemTime::now();
        File::open(&old)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(now - RETENTION - Duration::from_secs(1)))
            .unwrap();
        assert!(
            journal
                .sealed(&key().journal_stem(), RETENTION)
                .unwrap()
                .is_none()
        );
        assert!(old.exists());
        journal.expire(now).unwrap();
        assert!(!old.exists());
    }

    #[test]
    fn seal_expires_by_route_forget_while_younger_journal_file_survives_memory_age() {
        let dir = tempfile::tempdir().expect("tempdir");
        let memory_age = Duration::from_secs(7 * 86400);
        let forget = Duration::from_secs(86400);
        let now = SystemTime::now();
        let journal = Journal::open(dir.path(), memory_age, forget).expect("open");
        let stem = key().journal_stem();
        append(&journal, &stem, now, GRANT, "recent");
        journal.seal(&stem, now - 2 * forget).expect("seal");
        let seal_path = journal.seal_path(&stem);
        File::open(&seal_path)
            .expect("seal file")
            .set_modified(now - 2 * forget)
            .expect("age seal");
        let file_path = journal.path(&stem);
        File::open(&file_path)
            .expect("journal file")
            .set_modified(now - 2 * forget)
            .expect("age journal file");
        let recalled = journal
            .recall(
                &stem,
                GRANT,
                MemoryWindow {
                    forget_after: forget,
                    ..window()
                },
                now,
            )
            .expect("recall");
        assert_eq!(users(&recalled), ["recent"]);
        assert!(
            !seal_path.exists(),
            "route forget must expire the seal independently"
        );
        assert!(
            file_path.exists(),
            "memory age still protects the journal file"
        );
    }

    #[test]
    fn a_short_route_does_not_erase_a_long_routes_eligible_seal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let long = Duration::from_secs(7 * 86400);
        let short = Duration::from_secs(86400);
        let journal = Journal::open(dir.path(), long, long).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        journal.seal(&stem, now - 2 * short).expect("seal");
        File::open(journal.seal_path(&stem))
            .expect("seal file")
            .set_modified(now - 2 * short)
            .expect("age seal");
        assert!(journal.sealed(&stem, short).expect("short route").is_none());
        assert!(journal.sealed(&stem, long).expect("long route").is_some());
    }

    #[test]
    fn a_seal_expires_with_the_journal_retention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        journal.seal(&stem, now).unwrap();
        let path = journal.seal_path(&stem);
        File::open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(now - RETENTION - Duration::from_secs(1)))
            .unwrap();
        assert!(journal.sealed(&stem, RETENTION).unwrap().is_none());
        journal.expire(SystemTime::now()).unwrap();
        assert!(!path.exists());
    }
}
