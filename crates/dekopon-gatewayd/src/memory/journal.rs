#[expect(
    dead_code,
    reason = "v3 journal primitives are staged before the B4 switch"
)]
pub(crate) mod records;

mod selection;
pub(crate) use selection::{Projection, decide};
mod tools;
pub(crate) use tools::JournalMemoryTools;

use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dekopon_agent::prompt::{ConversationTurn, Exchange, History, Pin, ReadRef, Representation};
use dekopon_core::AgentId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    asset::{AssetRef, AssetSourceRef, PendingAsset, RecalledAsset},
    config::{MemoryWindow, memory::ResolvedPolicy},
    conversation::SealedConversation,
};

const EXTENSION: &str = "jsonl";
const SEAL_EXTENSION: &str = "sealed";
const FOLLOW_EXTENSION: &str = "followed";
const MAX_FOLLOW_BYTES: u64 = 4096;
const MAX_SEAL_BYTES: u64 = 4096;
const WHATSAPP_MEDIA_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

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
    #[serde(default)]
    original_bytes: u64,
    #[serde(default)]
    turn_id: u64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<AssetSourceRef>,
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
    pub prepared: Option<&'a super::generation::Prepared>,
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
    assets: Mutex<Option<std::sync::Weak<Mutex<crate::asset::Retention>>>>,
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
        let journal = Self::create(dir, max_age, retention, seal_retention)?;
        journal.reset_invalid_files()?;
        journal.expire(now)?;
        Ok(journal)
    }

    fn create(
        dir: &Path,
        max_age: Duration,
        retention: Duration,
        seal_retention: Duration,
    ) -> Result<Self, JournalError> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            max_age,
            agent_ages: HashMap::new(),
            agent_bytes: HashMap::new(),
            max_bytes_per_agent: 64 * 1024 * 1024,
            retention,
            seal_retention,
            lock: Mutex::new(()),
            assets: Mutex::new(None),
        })
    }

    pub(crate) fn attach_assets(
        &self,
        assets: &std::sync::Arc<Mutex<crate::asset::Retention>>,
    ) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        *self.assets.lock() = Some(std::sync::Arc::downgrade(assets));
        self.sync_assets()
    }

    fn finish_pending_assets(&self, stem: &str, next_id: u64) -> Result<(), JournalError> {
        self.sync_assets()?;
        if let Some(assets) = self
            .assets
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        {
            assets
                .lock()
                .finish_pending(stem, next_id)
                .map_err(|error| match error {
                    dekopon_model::asset::BlobError::Io(kind) => JournalError::Io { kind },
                    dekopon_model::asset::BlobError::TooLarge
                    | dekopon_model::asset::BlobError::Disabled
                    | dekopon_model::asset::BlobError::Capacity
                    | dekopon_model::asset::BlobError::Unknown
                    | dekopon_model::asset::BlobError::Reclaimed
                    | dekopon_model::asset::BlobError::Unauthorized
                    | dekopon_model::asset::BlobError::LengthChanged => JournalError::Corrupt,
                })?;
        }
        Ok(())
    }

    fn sync_assets(&self) -> Result<(), JournalError> {
        let Some(assets) = self
            .assets
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        else {
            return Ok(());
        };
        let mut references = HashSet::new();
        for agent in fs::read_dir(&self.dir)? {
            let agent = agent?.path();
            if !is_agent_directory(&agent) {
                continue;
            }
            for entry in fs::read_dir(&agent)? {
                let path = entry?.path();
                if !is_journal_file(&path) {
                    continue;
                }
                let stem = path
                    .strip_prefix(&self.dir)
                    .map_err(|_outside| JournalError::Corrupt)?
                    .with_extension("")
                    .to_string_lossy()
                    .into_owned();
                validated_lines(&path, |line| {
                    for asset in line.assets {
                        references.insert(crate::asset::StoredKey::new(
                            stem.clone(),
                            asset.id,
                            Duration::from_millis(asset.at_ms),
                            line.grant.clone(),
                        ));
                    }
                    Ok(())
                })?;
            }
        }
        assets
            .lock()
            .reconcile(&references)
            .map_err(|error| match error {
                dekopon_model::asset::BlobError::Io(kind) => JournalError::Io { kind },
                dekopon_model::asset::BlobError::TooLarge
                | dekopon_model::asset::BlobError::Disabled
                | dekopon_model::asset::BlobError::Capacity
                | dekopon_model::asset::BlobError::Unknown
                | dekopon_model::asset::BlobError::Reclaimed
                | dekopon_model::asset::BlobError::Unauthorized
                | dekopon_model::asset::BlobError::LengthChanged => JournalError::Corrupt,
            })
    }

    pub fn seal(&self, stem: &str, at: SystemTime) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        let _assets = AssetSync(self);
        let path = self.seal_path(stem);
        let temporary = path.with_extension("sealed.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, &Seal { at_ms: millis(at) }).map_err(io::Error::from)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }

    pub fn sealed(
        &self,
        stem: &str,
        forget_after: Duration,
    ) -> Result<Option<SealedConversation>, JournalError> {
        self.read_seal(stem, forget_after)
    }

    fn follow_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.{FOLLOW_EXTENSION}"))
    }

    pub(crate) fn write_follow(&self, key: &str, marker: FollowMarker) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        let _assets = AssetSync(self);
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
        let _assets = AssetSync(self);
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
        let _assets = AssetSync(self);
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
                    reason = "corrupt",
                    "gateway.follow"
                );
                continue;
            };
            if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                remove(&path);
                tracing::warn!(
                    event = "gateway_follow",
                    operation = "refused",
                    reason = "corrupt",
                    "gateway.follow"
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
                        reason = "corrupt",
                        "gateway.follow"
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
                count = overflow,
                "gateway.follow"
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
        parse_seal(file).map(Some)
    }

    pub fn recall(
        &self,
        stem: &str,
        grant: &str,
        window: MemoryWindow,
        now: SystemTime,
    ) -> Result<Recalled, JournalError> {
        let _lock = self.lock.lock();
        let _assets = AssetSync(self);
        self.sweep(now, false)?;
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
                            Some(AssetSourceRef::WhatsApp { .. })
                                if asset.at_ms < media_horizon_ms =>
                            {
                                None
                            }
                            source => source,
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
        _window: MemoryWindow,
    ) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        let _assets = AssetSync(self);
        let path = self.path(stem);
        let next_record_id = match Backward::open(&path).and_then(|mut lines| lines.next_line()) {
            Ok(last) => last.map_or(1, |last| last.next_record_id),
            Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            }) => 1,
            Err(JournalError::Corrupt) => {
                remove(&path);
                return Err(JournalError::Corrupt);
            }
            Err(error) => return Err(error),
        };
        let default;
        let prepared = match entry.prepared {
            Some(prepared) => prepared,
            None => {
                default = super::generation::prepare(
                    entry.turn,
                    &ResolvedPolicy::default(),
                    |_| None,
                    true,
                );
                &default
            }
        };
        let chat = prepared.original && prepared.include_chat;
        let mut line = Line {
            version: 2,
            at_ms: millis(entry.at),
            record_id: next_record_id,
            next_record_id: next_record_id.checked_add(1).ok_or(JournalError::Corrupt)?,
            source_eligibility: prepared.original,
            original_bytes: super::generation::captured_bytes(entry.turn, prepared.include_chat),
            turn_id: entry.turn.memory_id().unwrap_or(0),
            representations: prepared.representations.clone(),
            exchanges: prepared.exchanges.clone(),
            pins: Vec::new(),
            reads: Vec::new(),
            availability: prepared.availability.clone(),
            grant: entry.grant.to_owned(),
            user: chat.then(|| entry.turn.user().to_owned()),
            answer: entry.turn.answer().filter(|_| chat).map(str::to_owned),
            assets: entry
                .inventory
                .iter()
                .filter_map(|asset| {
                    if matches!(&asset.source, Some(AssetSourceRef::Generated { .. })) {
                        return None;
                    }
                    Some(LineAsset {
                        id: asset.id,
                        at_ms: millis(asset.arrived),
                        name: asset.name.clone(),
                        mime: asset.mime.clone(),
                        size: asset.size,
                        source: asset.source.clone(),
                    })
                })
                .collect(),
            next_asset_id: entry.next_asset_id,
        };
        for representation in &mut line.representations {
            representation.source_record_id = next_record_id;
        }
        for exchange in &mut line.exchanges {
            for representation in exchange.request.iter_mut().chain(&mut exchange.result) {
                representation.source_record_id = next_record_id;
            }
        }
        let encoded = encode_line(&line)?;
        let agent = stem.split_once('/').map(|(agent, _)| agent);
        if agent.is_some_and(|agent| encoded.len() as u64 > self.agent_cap(agent)) {
            tracing::warn!(
                event = "gateway_memory_retention_failed",
                reason = "agent-byte-cap",
                encoded_bytes = encoded.len(),
                "gateway.memory.retention.failed"
            );
            return Ok(());
        }
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
        file.write_all(&encoded)?;
        // Ownership transfers before fallible scans can encounter another corrupt journal file.
        if let Some(assets) = self
            .assets
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        {
            assets.lock().retain_record(line.assets.iter().map(|asset| {
                crate::asset::StoredKey::new(
                    stem.to_owned(),
                    asset.id,
                    Duration::from_millis(asset.at_ms),
                    line.grant.clone(),
                )
            }));
        }
        file.sync_all()?;
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        File::open(&self.dir)?.sync_all()?;
        drop(file);
        let maintained = (|| -> Result<(), JournalError> {
            let horizon = millis(entry.at).saturating_sub(duration_millis(self.age_for(stem)));
            match sweep_file(&path, horizon) {
                Err(JournalError::Corrupt) => {
                    remove(&path);
                    return Err(JournalError::Corrupt);
                }
                result => result?,
            };
            if let Some(agent) = agent
                && self.enforce_agent_cap(agent, Some(&path))?
            {
                tracing::warn!(
                    event = "gateway_memory_retention_failed",
                    reason = "agent-byte-cap",
                    encoded_bytes = encoded.len(),
                    "gateway.memory.retention.failed"
                );
            }
            self.finish_pending_assets(stem, entry.next_asset_id)?;
            Ok(())
        })();
        if let Err(error) = maintained {
            tracing::warn!(reason = error.label(), %error, "gateway.asset.reclaim.failed");
        }
        Ok(())
    }

    pub(crate) fn enforce_caps(&self) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        let _assets = AssetSync(self);
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
        appended: Option<&Path>,
    ) -> Result<bool, JournalError> {
        let limit = self.agent_cap(agent);
        let directory = self.dir.join(agent);
        if !directory.exists() {
            return Ok(false);
        }
        let mut files = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if is_journal_file(&path) {
                let size = fs::metadata(&path)?.len();
                total = total.saturating_add(size);
                files.push((path, size, 0u64, None));
            }
        }
        if total <= limit {
            return Ok(false);
        }
        for (path, _, _, head) in &mut files {
            *head = head_at(path, 0)?;
        }
        while total > limit {
            let Some((path, _, cut, head)) = files
                .iter_mut()
                .filter(|(_, _, _, head)| head.is_some())
                .min_by(|left, right| {
                    (left.3.map(|(at, _)| at), &left.0).cmp(&(right.3.map(|(at, _)| at), &right.0))
                })
            else {
                break;
            };
            let Some((_, end)) = *head else {
                break;
            };
            total = total.saturating_sub(end - *cut);
            *cut = end;
            *head = head_at(path, end)?;
        }
        for (path, size, cut, _) in &files {
            if *cut == 0 {
                continue;
            }
            if *cut >= *size {
                fs::remove_file(path)?;
            } else {
                keep_from(path, *cut)?;
            }
        }
        Ok(appended.is_some_and(|appended| {
            files
                .iter()
                .any(|(path, size, cut, _)| path == appended && cut >= size)
        }))
    }

    fn agent_cap(&self, agent: &str) -> u64 {
        self.agent_bytes
            .get(agent)
            .copied()
            .unwrap_or(self.max_bytes_per_agent)
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
                        && matches!(
                            validated_lines(&file.path(), |_line| Ok(())),
                            Err(JournalError::Corrupt)
                        )
                    {
                        remove(&file.path());
                        tracing::warn!(
                            event = "gateway_journal_reset",
                            reason = "corrupt",
                            "gateway.journal.reset"
                        );
                    }
                }
            } else if is_journal_file(&path) && !path.with_extension(FOLLOW_EXTENSION).exists() {
                remove(&path);
                tracing::warn!(
                    event = "gateway_journal_reset",
                    reason = "legacy",
                    "gateway.journal.reset"
                );
            }
        }
        Ok(())
    }

    fn expire(&self, now: SystemTime) -> Result<(), JournalError> {
        self.sweep(now, true)
    }

    // Follow markers expire by their lifecycle deadline, not the journal window's file mtime.
    fn sweep(&self, now: SystemTime, records: bool) -> Result<(), JournalError> {
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
                            tracing::info!(
                                event = "gateway_journal_expired",
                                "gateway.journal.expired"
                            );
                        } else if records {
                            let age = self.age_for(&format!(
                                "{}/{}",
                                path.file_name().unwrap_or_default().to_string_lossy(),
                                file_path.file_stem().unwrap_or_default().to_string_lossy()
                            ));
                            let horizon = millis(now).saturating_sub(duration_millis(age));
                            match sweep_file(&file_path, horizon) {
                                Ok(true) => tracing::info!(
                                    event = "gateway_journal_expired",
                                    "gateway.journal.expired"
                                ),
                                Err(JournalError::Corrupt) => {
                                    remove(&file_path);
                                    tracing::warn!(
                                        event = "gateway_journal_reset",
                                        reason = "corrupt",
                                        "gateway.journal.reset"
                                    );
                                }
                                Err(error) => return Err(error),
                                Ok(false) => {}
                            }
                        }
                    }
                }
                continue;
            }
            let stale = match path.extension() {
                Some(extension) if extension == SEAL_EXTENSION => {
                    let modified = entry.metadata()?.modified()?;
                    if seal_cutoff.is_some_and(|limit| modified < limit) {
                        true
                    } else {
                        match File::open(&path)
                            .map_err(JournalError::from)
                            .and_then(parse_seal)
                        {
                            Err(JournalError::Corrupt) => {
                                remove(&path);
                                tracing::warn!(
                                    event = "gateway_journal_reset",
                                    reason = "corrupt",
                                    "gateway.journal.reset"
                                );
                                false
                            }
                            Err(JournalError::Io {
                                kind: io::ErrorKind::NotFound,
                            }) => false,
                            Err(error) => return Err(error),
                            Ok(_) => false,
                        }
                    }
                }
                Some(extension) if extension == FOLLOW_EXTENSION => {
                    read_follow_marker(&path)?.is_some_and(|marker| marker.until_ms <= millis(now))
                }
                _ => false,
            };
            if stale && remove(&path) {
                if records
                    && path
                        .extension()
                        .is_some_and(|extension| extension == FOLLOW_EXTENSION)
                {
                    tracing::info!(
                        audit.event = "gateway.follow",
                        operation = "banished",
                        cause = "lapse",
                        "gateway.follow"
                    );
                }
                tracing::info!(event = "gateway_journal_expired", "gateway.journal.expired");
            }
        }
        Ok(())
    }
}

struct AssetSync<'a>(&'a Journal);
impl Drop for AssetSync<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.0.sync_assets() {
            tracing::warn!(reason = error.label(), %error, "gateway.asset.reclaim.failed");
        }
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

fn parse_seal(file: File) -> Result<SealedConversation, JournalError> {
    let mut bytes = Vec::new();
    file.take(MAX_SEAL_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SEAL_BYTES {
        return Err(JournalError::Corrupt);
    }
    let seal = serde_json::from_slice::<Seal>(&bytes).map_err(|_invalid| JournalError::Corrupt)?;
    UNIX_EPOCH
        .checked_add(Duration::from_millis(seal.at_ms))
        .map(|at| SealedConversation { at })
        .ok_or(JournalError::Corrupt)
}

fn remove(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            tracing::warn!(event = "gateway_journal_remove_failed", kind = %error.kind(), "gateway.journal.remove.failed");
            false
        }
    }
}

pub(crate) fn mint_turn_id() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = millis(SystemTime::now()).saturating_mul(1_000_000);
    let next = |last: u64| now.max(last.saturating_add(1));
    LAST.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
        Some(next(last))
    })
    .map_or(now, next)
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

fn admits(line: &Line, grant: &str, horizon_ms: u64, sealed_at_ms: Option<u64>) -> bool {
    line.grant == grant
        && line.at_ms >= horizon_ms
        && sealed_at_ms.is_none_or(|seal| line.at_ms > seal)
}

fn encode_line(line: &Line) -> Result<Vec<u8>, JournalError> {
    let mut encoded = serde_json::to_vec(line).map_err(io::Error::from)?;
    encoded.push(b'\n');
    Ok(encoded)
}

pub(super) fn millis(at: SystemTime) -> u64 {
    duration_millis(at.duration_since(UNIX_EPOCH).unwrap_or_default())
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn checked(line: Line) -> Result<Line, JournalError> {
    if line.version != 2
        || line.next_record_id != line.record_id.saturating_add(1)
        || (!line.source_eligibility && (line.user.is_some() || line.answer.is_some()))
    {
        return Err(JournalError::Corrupt);
    }
    Ok(line)
}

fn body(line: &Line, id: &str, version: &str) -> Option<String> {
    let matches = |rep: &&Representation| rep.routine_id == id && rep.version == version;
    let mut reps = line.representations.iter().filter(matches).peekable();
    let original = reps.peek()?.kind == "original";
    let mut text = if original {
        if !line.source_eligibility {
            return None;
        }
        match (&line.user, &line.answer) {
            (Some(user), Some(answer)) => format!("{user}\n{answer}"),
            (Some(user), None) => user.clone(),
            (None, _) => String::new(),
        }
    } else {
        reps.filter_map(|rep| rep.text.as_deref()).collect()
    };
    if text.is_empty() && (!original || line.exchanges.is_empty()) {
        return None;
    }
    for exchange in &line.exchanges {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!(
            "[tool: {}, outcome: {}, exit: {:?}]",
            exchange.tool, exchange.outcome, exchange.exit_status
        ));
        for (label, parts) in [("request", &exchange.request), ("result", &exchange.result)] {
            let content: String = parts
                .iter()
                .filter(matches)
                .filter_map(|rep| rep.text.as_deref())
                .collect();
            if !content.is_empty() {
                text.push_str(&format!("\n[{label}] {content}"));
            }
        }
    }
    Some(text)
}

struct Backward {
    file: File,
    end: u64,
    newer: Option<u64>,
}

impl Backward {
    fn open(path: &Path) -> Result<Self, JournalError> {
        let file = File::open(path)?;
        let end = file.metadata()?.len();
        Ok(Self {
            file,
            end,
            newer: None,
        })
    }

    fn next_line(&mut self) -> Result<Option<Line>, JournalError> {
        let Some(stop) = self.end.checked_sub(1) else {
            return Ok(None);
        };
        let mut chunk = [0; 8192];
        let mut cursor = stop;
        let mut start = 0;
        while cursor > 0 {
            let from = cursor.saturating_sub(chunk.len() as u64);
            let part = &mut chunk[..(cursor - from) as usize];
            self.file.seek(SeekFrom::Start(from))?;
            self.file.read_exact(part)?;
            if let Some(index) = part.iter().rposition(|byte| *byte == b'\n') {
                start = from + index as u64 + 1;
                break;
            }
            cursor = from;
        }
        let mut bytes = vec![0; (stop - start) as usize];
        self.file.seek(SeekFrom::Start(start))?;
        self.file.read_exact(&mut bytes)?;
        self.end = start;
        let line = checked(
            serde_json::from_slice::<Line>(&bytes).map_err(|_malformed| JournalError::Corrupt)?,
        )?;
        if self.newer.is_some_and(|newer| newer != line.next_record_id) {
            return Err(JournalError::Corrupt);
        }
        self.newer = Some(line.record_id);
        Ok(Some(line))
    }
}

fn head_at(path: &Path, offset: u64) -> Result<Option<(u64, u64)>, JournalError> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut lines = serde_json::Deserializer::from_reader(BufReader::new(file)).into_iter::<Line>();
    let Some(line) = lines
        .next()
        .transpose()
        .map_err(|_malformed| JournalError::Corrupt)?
    else {
        return Ok(None);
    };
    #[cfg(test)]
    observe_live_decoded(1);
    let line = checked(line)?;
    Ok(Some((line.at_ms, offset + lines.byte_offset() as u64 + 1)))
}

fn read_window(
    path: &Path,
    grant: &str,
    window: MemoryWindow,
    seal_ms: Option<u64>,
    horizon_ms: u64,
    age_horizon_ms: u64,
) -> Result<(History, Option<Line>), JournalError> {
    let mut lines = Backward::open(path)?;
    let mut turns = Vec::new();
    let mut bytes = 0_usize;
    let mut records = 0_usize;
    let mut last = None;
    while let Some(mut line) = lines.next_line()? {
        #[cfg(test)]
        observe_live_decoded(1 + usize::from(last.is_some()));
        if !admits(&line, grant, horizon_ms.max(age_horizon_ms), seal_ms) {
            break;
        }
        records += 1;
        let turn = line.user.take().map(|user| {
            match line.answer.take() {
                Some(answer) => ConversationTurn::completed(user, answer),
                None => ConversationTurn::unanswered(user),
            }
            .with_memory_id(line.turn_id)
            .clipped(window.limits.max_bytes)
        });
        last.get_or_insert(line);
        if let Some(turn) = turn {
            bytes = bytes.saturating_add(turn.bytes());
            if bytes > window.limits.max_bytes {
                break;
            }
            turns.push(turn);
        }
        if records >= window.limits.max_turns {
            break;
        }
    }
    Ok((
        History::from_turns(window.limits, turns.into_iter().rev()),
        last,
    ))
}

#[cfg(test)]
thread_local! {
    static PEAK_LIVE_DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static HELD_DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn observe_live_decoded(count: usize) {
    HELD_DECODED.with(|held| {
        PEAK_LIVE_DECODED.with(|peak| peak.set(peak.get().max(count + held.get())));
    });
}

#[cfg(test)]
struct DecodedHold;

#[cfg(test)]
impl DecodedHold {
    fn one() -> Self {
        HELD_DECODED.with(|held| held.set(held.get() + 1));
        Self
    }
}

#[cfg(test)]
impl Drop for DecodedHold {
    fn drop(&mut self) {
        HELD_DECODED.with(|held| held.set(held.get() - 1));
    }
}

#[cfg(test)]
fn read_lines(path: &Path) -> Result<Vec<Line>, JournalError> {
    let mut lines = Vec::new();
    validated_lines(path, |line| {
        lines.push(line);
        Ok(())
    })?;
    observe_live_decoded(lines.len());
    Ok(lines)
}

fn validated_lines(
    path: &Path,
    mut visit: impl FnMut(Line) -> Result<(), JournalError>,
) -> Result<(), JournalError> {
    let file = BufReader::new(File::open(path)?);
    let mut previous = None;
    for item in serde_json::Deserializer::from_reader(file).into_iter::<Line>() {
        let line = checked(item.map_err(|_malformed| JournalError::Corrupt)?)?;
        if previous.is_some_and(|id| id != line.record_id) {
            return Err(JournalError::Corrupt);
        }
        previous = Some(line.next_record_id);
        #[cfg(test)]
        observe_live_decoded(1);
        visit(line)?;
    }
    Ok(())
}

fn sweep_file(path: &Path, horizon: u64) -> Result<bool, JournalError> {
    if head_at(path, 0)?.is_none_or(|(at, _)| at >= horizon) {
        return Ok(false);
    }
    rewrite_eligible(path, horizon)?;
    if fs::metadata(path)?.len() == 0 {
        remove(path);
    }
    Ok(true)
}

fn replace_with(
    path: &Path,
    fill: impl FnOnce(&mut File) -> Result<(), JournalError>,
) -> Result<(), JournalError> {
    let temporary = path.with_extension("compact");
    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        fill(&mut file)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        remove(&temporary);
    }
    result
}

fn rewrite_eligible(path: &Path, horizon: u64) -> Result<(), JournalError> {
    replace_with(path, |file| {
        validated_lines(path, |line| {
            if line.at_ms >= horizon {
                file.write_all(&encode_line(&line)?)?;
            }
            Ok(())
        })
    })
}

fn keep_from(path: &Path, offset: u64) -> Result<(), JournalError> {
    replace_with(path, |file| {
        let mut source = File::open(path)?;
        source.seek(SeekFrom::Start(offset))?;
        io::copy(&mut source, file)?;
        Ok(())
    })
}

#[cfg(test)]
fn rewrite(path: &Path, lines: &[Line]) -> Result<(), JournalError> {
    replace_with(path, |file| {
        for line in lines {
            file.write_all(&encode_line(line)?)?;
        }
        Ok(())
    })
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
                    prepared: None,
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
            .assets_for_access(&access, pending, Instant::now())
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

    fn corrupt_oldest_line_in_place(path: &Path) {
        let bytes = fs::read(path).expect("journal bytes");
        let end = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .expect("first record");
        let mut file = File::options()
            .write(true)
            .open(path)
            .expect("open first record");
        file.write_all(&vec![b'!'; end])
            .expect("overwrite first record in place");
    }

    #[test]
    fn cold_recall_clips_the_newest_turn_and_never_reads_a_corrupt_older_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let mut limits = window();
        limits.limits.max_turns = 40;
        limits.limits.max_bytes = 1000;
        append_turn(
            &journal,
            &stem,
            now,
            GRANT,
            &ConversationTurn::completed("O", "old"),
            &[],
            limits,
        );
        append_turn(
            &journal,
            &stem,
            now,
            GRANT,
            &ConversationTurn::completed("1".repeat(300), "a".repeat(300)),
            &[],
            limits,
        );
        let newest = ConversationTurn::completed("2".repeat(1500), "b".repeat(100));
        append_turn(&journal, &stem, now, GRANT, &newest, &[], limits);
        let path = journal.path(&stem);
        corrupt_oldest_line_in_place(&path);
        PEAK_LIVE_DECODED.with(|peak| peak.set(0));
        let recalled = journal
            .recall(&stem, GRANT, limits, now)
            .expect("bounded cold recall");
        let mut warm = History::new(limits.limits);
        warm.record(newest.clone().with_memory_id(0));
        assert_eq!(
            recalled.history.turns(),
            [newest.with_memory_id(0).clipped(1000)]
        );
        assert_eq!(recalled.history, warm);
        assert_eq!(recalled.history.bytes(), 1000);
        assert!(path.exists());
        PEAK_LIVE_DECODED.with(|peak| assert!(peak.get() <= 2));
    }

    #[test]
    fn cold_recall_counts_userless_records_before_a_corrupt_older_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let mut limits = window();
        limits.limits.max_turns = 3;
        append_turn(
            &journal,
            &stem,
            now,
            GRANT,
            &ConversationTurn::completed("O", "old"),
            &[],
            limits,
        );
        for _ in 0..3 {
            append_turn(
                &journal,
                &stem,
                now,
                GRANT,
                &ConversationTurn::unanswered(""),
                &[],
                limits,
            );
        }
        let path = journal.path(&stem);
        let mut lines = read_lines(&path).expect("records");
        for line in lines.iter_mut().skip(1) {
            line.user = None;
            line.answer = None;
            line.source_eligibility = false;
        }
        let encoded: Vec<u8> = lines
            .iter()
            .flat_map(|line| encode_line(line).expect("v2 record"))
            .collect();
        fs::write(&path, encoded).expect("three eligible userless records");
        corrupt_oldest_line_in_place(&path);
        let recalled = journal
            .recall(&stem, GRANT, limits, now)
            .expect("bounded userless recall");
        assert!(recalled.history.is_empty());
        assert!(path.exists());
    }

    #[test]
    fn cold_recall_and_old_record_lookup_decode_at_most_two_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        let journal = Journal::open_with_age(
            dir.path(),
            Duration::from_secs(3600),
            Duration::from_secs(7200),
            RETENTION,
        )
        .expect("open");
        append(
            &journal,
            &stem,
            now - Duration::from_secs(5400),
            GRANT,
            "expired",
        );
        append_versioned(&journal, &stem, now - Duration::from_secs(20), "turn-0");
        for index in 1..100 {
            append(
                &journal,
                &stem,
                now - Duration::from_secs(20),
                GRANT,
                &format!("turn-{index}"),
            );
        }
        PEAK_LIVE_DECODED.with(|peak| peak.set(0));
        let recalled = journal.recall(&stem, GRANT, window(), now).expect("recall");
        assert_eq!(users(&recalled).last().copied(), Some("turn-99"));
        PEAK_LIVE_DECODED.with(|peak| {
            assert_eq!(
                peak.get(),
                2,
                "recall including expiry must count both retained and incoming records"
            )
        });
        PEAK_LIVE_DECODED.with(|peak| peak.set(0));
        use dekopon_agent::memory_tools::MemoryTools;
        let tools = JournalMemoryTools {
            journal: &journal,
            stem: stem.clone(),
            grant: GRANT.into(),
            versions: [("original".into(), "v1".into())].into(),
        };
        assert!(tools.read(2, "original", None).starts_with("turn-0"));
        PEAK_LIVE_DECODED.with(|peak| {
            assert_eq!(
                peak.get(),
                2,
                "lookup must count the retained match and current record"
            )
        });
    }

    fn append_versioned(journal: &Journal, stem: &str, now: SystemTime, user: &str) {
        append(journal, stem, now, GRANT, user);
        let mut lines = read_lines(&journal.path(stem)).expect("lines");
        let last = lines.last_mut().expect("appended");
        last.representations.push(Representation {
            routine_id: "original".into(),
            version: "v1".into(),
            source_record_id: last.record_id,
            part: None,
            kind: "original".into(),
            captured_bytes: user.len() as u64,
            text: None,
        });
        rewrite(&journal.path(stem), &lines).expect("version");
    }

    #[test]
    fn a_seal_refuses_record_reads_and_pins_through_shared_admission() {
        use dekopon_agent::memory_tools::MemoryTools;
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
        let stem = key().journal_stem();
        let at = SystemTime::now();
        append_versioned(&journal, &stem, at, "sealed-canary");
        let tools = JournalMemoryTools {
            journal: &journal,
            stem: stem.clone(),
            grant: GRANT.into(),
            versions: [("original".into(), "v1".into())].into(),
        };
        assert!(tools.read(1, "original", None).contains("sealed-canary"));
        journal.seal(&stem, at).unwrap();
        assert_eq!(
            tools.read(1, "original", None),
            "record unavailable: expired or changed grant"
        );
        assert_eq!(
            tools.set_pin(1, "original"),
            "record unavailable: expired or changed grant"
        );
        assert!(
            journal
                .recall(&stem, GRANT, window(), at)
                .unwrap()
                .history
                .is_empty()
        );
    }

    #[test]
    fn memory_tools_read_references_and_pins_survive_restart_without_copying_text() {
        use dekopon_agent::memory_tools::MemoryTools;
        let dir = tempfile::tempdir().expect("tempdir");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        {
            let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
            append_versioned(&journal, &stem, now, "read-line-sentinel");
            append_versioned(&journal, &stem, now, "second");
            let tools = JournalMemoryTools {
                journal: &journal,
                stem: stem.clone(),
                grant: GRANT.into(),
                versions: [("original".into(), "v1".into())].into(),
            };
            assert_eq!(tools.read(1, "original", Some((0, 4))), "read");
            assert_eq!(
                tools.set_pin(1, "original"),
                "pinned record 1 to original; pinned text bytes: 41"
            );
            assert_eq!(
                read_lines(&journal.path(&stem)).expect("lines")[0].reads[0].range,
                Some((0, 4))
            );
        }
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("reopen");
        let lines = read_lines(&journal.path(&stem)).expect("lines");
        assert_eq!(lines[0].pins[0].record_id, 1);
        assert_eq!(lines[0].reads[0].outcome, "read");
        assert!(
            !serde_json::to_string(&lines[0].reads)
                .expect("references")
                .contains("read-line-sentinel")
        );
        let tools = JournalMemoryTools {
            journal: &journal,
            stem,
            grant: GRANT.into(),
            versions: [("original".into(), "v1".into())].into(),
        };
        assert!(tools.set_pin(2, "original").starts_with("pinned record 2"));
        assert_eq!(tools.clear_pin(Some(1)), "pin cleared");
        let lines = read_lines(&journal.path(&tools.stem)).expect("lines");
        assert!(lines[0].pins.is_empty());
        assert_eq!(lines[1].pins[0].record_id, 2);
        assert_eq!(
            tools.set_pin(1, "original"),
            "pinned record 1 to original; pinned text bytes: 41"
        );
        assert_eq!(tools.clear_pin(None), "pin cleared");
        assert!(
            read_lines(&journal.path(&tools.stem))
                .expect("lines")
                .iter()
                .all(|line| line.pins.is_empty())
        );
    }

    #[test]
    fn memory_tools_refuse_changed_grants_expiry_mismatched_version_and_invalid_ranges() {
        use dekopon_agent::memory_tools::MemoryTools;
        let dir = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let stem = key().journal_stem();
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        append_versioned(&journal, &stem, now, "canary");
        let mut tools = JournalMemoryTools {
            journal: &journal,
            stem: stem.clone(),
            grant: GRANT.into(),
            versions: [("original".into(), "v1".into())].into(),
        };
        assert!(
            tools
                .read(1, "original", Some((4, 2)))
                .starts_with("range unavailable")
        );
        assert_eq!(
            read_lines(&journal.path(&stem)).expect("lines")[0].reads[0].outcome,
            "invalid-range"
        );
        tools.versions.insert("original".into(), "v2".into());
        let refusal = tools.read(1, "original", None);
        assert!(refusal.starts_with("representation unavailable"));
        assert!(!refusal.contains("canary"));
        assert!(
            tools
                .set_pin(1, "original")
                .starts_with("representation unavailable")
        );
        tools.grant = "different".into();
        assert!(
            tools
                .read(1, "original", None)
                .starts_with("record unavailable")
        );
        assert!(
            tools
                .set_pin(1, "original")
                .starts_with("record unavailable")
        );
        tools.grant = GRANT.into();
        assert!(
            tools
                .read(999, "original", None)
                .starts_with("record unavailable")
        );
        journal
            .expire(now + Duration::from_secs(7201))
            .expect("expire");
        assert!(
            tools
                .read(1, "original", None)
                .starts_with("record unavailable")
        );
    }

    #[test]
    fn a_summary_only_record_reads_without_resurrecting_its_original() {
        use dekopon_agent::memory_tools::MemoryTools;
        let dir = tempfile::tempdir().expect("tempdir");
        let now = SystemTime::now();
        let stem = key().journal_stem();
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        append(&journal, &stem, now, GRANT, "disposed-source-sentinel");
        let mut lines = read_lines(&journal.path(&stem)).expect("lines");
        let line = &mut lines[0];
        line.user = None;
        line.answer = None;
        line.source_eligibility = false;
        line.availability = vec!["summary".into()];
        line.representations.push(Representation {
            routine_id: "summary".into(),
            version: "v1".into(),
            source_record_id: 1,
            part: None,
            kind: "prefix".into(),
            captured_bytes: 100,
            text: Some("retained summary".into()),
        });
        rewrite(&journal.path(&stem), &lines).expect("rewrite");
        let tools = JournalMemoryTools {
            journal: &journal,
            stem: stem.clone(),
            grant: GRANT.into(),
            versions: [
                ("summary".into(), "v1".into()),
                ("original".into(), "v1".into()),
            ]
            .into(),
        };
        assert_eq!(tools.read(1, "summary", None), "retained summary");
        assert_eq!(
            tools.set_pin(1, "summary"),
            "pinned record 1 to summary; pinned text bytes: 16"
        );
        assert!(
            tools
                .read(1, "original", None)
                .starts_with("representation unavailable")
        );
        assert!(
            !fs::read_to_string(journal.path(&stem))
                .expect("disk")
                .contains("disposed-source-sentinel")
        );
        assert_eq!(
            read_lines(&journal.path(&stem)).expect("lines")[0].reads[0].source_record_id,
            1
        );
    }

    #[test]
    fn memory_tools_are_scoped_to_full_shared_or_private_conversation_keys() {
        use dekopon_agent::memory_tools::MemoryTools;
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let agent = "reviewer".parse().expect("agent");
        let shared = ConversationKey::shared(&agent, "whatsapp-test", "chat").journal_stem();
        let private = ConversationKey::private(
            &agent,
            "whatsapp-test",
            "chat",
            &"tel.16034700182".parse().expect("subject"),
        )
        .journal_stem();
        append_versioned(&journal, &shared, SystemTime::now(), "shared-record");
        let make = |stem| JournalMemoryTools {
            journal: &journal,
            stem,
            grant: GRANT.into(),
            versions: [("original".into(), "v1".into())].into(),
        };
        assert!(
            make(private)
                .read(1, "original", None)
                .starts_with("record unavailable")
        );
        let shared_reader = make(shared.clone());
        assert!(
            shared_reader
                .set_pin(1, "original")
                .starts_with("pinned record")
        );
        assert!(
            make(shared)
                .read(1, "original", None)
                .starts_with("shared-record")
        );
        assert_eq!(
            read_lines(&journal.path(&shared_reader.stem)).expect("lines")[0].pins[0].record_id,
            1
        );
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
    fn a_source_less_row_survives_two_append_reopen_cycles() {
        for source in [None, Some(whatsapp(0))] {
            let dir = tempfile::tempdir().unwrap();
            let stem = key().journal_stem();
            let at = UNIX_EPOCH + Duration::from_millis(millis(SystemTime::now()));
            let mut inventory = photos(2, whatsapp);
            inventory[0].source = source;
            let arrived = if inventory[0].source.is_some() {
                at - WHATSAPP_MEDIA_LIFETIME - Duration::from_millis(1)
            } else {
                at
            };
            inventory[0].arrived = arrived;
            inventory[1].source = Some(AssetSourceRef::Generated {
                capability: "image.generate".into(),
                invocation: "generated".into(),
            });
            let window = MemoryWindow {
                forget_after: Duration::from_secs(30 * 24 * 60 * 60),
                ..window()
            };
            let turn = ConversationTurn::completed("attachments", "kept");
            for _ in 0..2 {
                let journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
                journal
                    .append(
                        &stem,
                        &Entry {
                            at,
                            grant: GRANT,
                            turn: &turn,
                            inventory: &inventory,
                            next_asset_id: 3,
                            prepared: None,
                        },
                        window,
                    )
                    .unwrap();
                drop(journal);
                let journal = Journal::open(dir.path(), RETENTION, RETENTION).unwrap();
                let recalled = journal.recall(&stem, GRANT, window, at).unwrap();
                assert_eq!(recalled.next_asset_id, 3);
                assert_eq!(recalled.assets.len(), 1);
                let row = &recalled.assets[0];
                assert_eq!(row.id, 1);
                assert_eq!(row.asset.name, "photo-0.jpg");
                assert_eq!(row.asset.mime, "image/jpeg");
                assert_eq!(row.asset.size, Some(1000));
                assert_eq!(row.arrived, arrived);
                assert!(row.asset.source.is_none());
                let store = AssetStore::new(8, RETENTION);
                let access = AssetAccess::persistent(key(), 1, Arc::new(AssetFence::new()));
                store.restore(
                    &access,
                    recalled.assets,
                    recalled.next_asset_id,
                    Instant::now(),
                );
                assert_eq!(access.next_asset_id(), 3);
                inventory = store.inventory(&access);
                drop(journal);
            }
        }
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
    fn a_large_file_appends_without_rewriting_eligible_turns() {
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
        assert!(bytes > 64 * 1024, "{bytes} bytes");
        let before = fs::metadata(journal.path(&stem)).expect("file");
        PEAK_LIVE_DECODED.with(|peak| peak.set(0));
        append_with(&journal, &stem, now, GRANT, &filler, &inventory);
        PEAK_LIVE_DECODED.with(|peak| {
            assert!(
                peak.get() <= 2,
                "append must decode only its own tail and head"
            )
        });
        let after = fs::metadata(journal.path(&stem)).expect("file");
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(
            after.ino(),
            before.ino(),
            "an eligible tail append must not rewrite the file"
        );
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
        assert!(matches!(
            journal.recall(&stem, GRANT, window(), now),
            Err(JournalError::Corrupt)
        ));
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
        rewrite_eligible(
            &journal.path(&stem),
            millis(now + age).saturating_sub(duration_millis(age)),
        )
        .unwrap();
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
        assert_eq!(
            record
                .representations
                .last()
                .and_then(|rep| rep.text.as_deref()),
            Some("summary")
        );
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
            rewrite_eligible(
                &path,
                millis(now).saturating_sub(duration_millis(RETENTION))
            ),
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
    fn a_corrupt_seal_is_refused_without_mutating_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let path = journal.seal_path(&stem);
        fs::write(&path, b"not a marker").unwrap();
        assert!(matches!(
            journal.sealed(&stem, RETENTION),
            Err(JournalError::Corrupt)
        ));
        assert!(path.exists());
    }

    #[test]
    fn an_oversized_seal_is_refused_without_mutating_the_file() {
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
        assert!(path.exists());
    }

    #[test]
    fn a_corrupt_seal_survives_an_unlocked_read_and_is_removed_by_maintenance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let corrupt = journal.seal_path(&key().journal_stem());
        let mut oversized = br#"{"atMs":1}"#.to_vec();
        oversized.resize((MAX_SEAL_BYTES + 1) as usize, b' ');
        let oversized_path = journal.seal_path("oversized");
        let valid = journal.seal_path("valid");
        fs::write(&corrupt, b"not a marker").unwrap();
        fs::write(&oversized_path, oversized).unwrap();
        fs::write(&valid, br#"{"atMs":1}"#).unwrap();
        assert!(journal.sealed(&key().journal_stem(), RETENTION).is_err());
        assert!(corrupt.exists());
        journal.expire(SystemTime::now()).unwrap();
        assert!(!corrupt.exists());
        assert!(!oversized_path.exists());
        assert!(valid.exists());
        fs::write(&corrupt, b"not a marker").unwrap();
        drop(journal);
        Journal::open(dir.path(), RETENTION, RETENTION).expect("reopen");
        assert!(!corrupt.exists());
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

    fn tear_last_line(path: &Path) {
        let mut file = OpenOptions::new().append(true).open(path).expect("journal");
        file.write_all(br#"{"version":2,"atMs":1"#)
            .expect("torn line");
    }

    #[test]
    fn a_torn_last_line_resets_the_file_at_append_so_the_next_append_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let now = SystemTime::now();
        append(&journal, &stem, now, GRANT, "before");
        tear_last_line(&journal.path(&stem));
        let turn = ConversationTurn::completed("lost", "re: lost");
        let entry = Entry {
            prepared: None,
            at: now,
            grant: GRANT,
            turn: &turn,
            inventory: &[],
            next_asset_id: 1,
        };
        assert!(matches!(
            journal.append(&stem, &entry, window()),
            Err(JournalError::Corrupt)
        ));
        assert!(!journal.path(&stem).exists());
        append(&journal, &stem, now, GRANT, "after");
        assert_eq!(
            users(&journal.recall(&stem, GRANT, window(), now).expect("recall")),
            ["after"]
        );
    }

    #[test]
    fn a_torn_last_line_resets_the_file_at_projection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let policy = ResolvedPolicy::default();
        append(&journal, &stem, SystemTime::now(), GRANT, "before");
        tear_last_line(&journal.path(&stem));
        let projected = journal.project(
            Projection {
                stem: &stem,
                grant: GRANT,
                window: window(),
                sealed_at: None,
                policy: &policy,
            },
            &History::new(window().limits),
            |_available, _evidence| Err("unused"),
        );
        assert!(matches!(projected, Err(JournalError::Corrupt)));
        assert!(!journal.path(&stem).exists());
    }

    #[test]
    fn a_turn_larger_than_the_agent_cap_is_refused_without_evicting_older_turns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let now = SystemTime::now();
        let stem = key().journal_stem();
        let other =
            ConversationKey::shared(&"reviewer".parse().unwrap(), "slack", "other").journal_stem();
        append(&journal, &stem, now - Duration::from_secs(2), GRANT, "kept");
        append(
            &journal,
            &other,
            now - Duration::from_secs(1),
            GRANT,
            "also kept",
        );
        let before = [
            fs::read(journal.path(&stem)).unwrap(),
            fs::read(journal.path(&other)).unwrap(),
        ];
        let cap = before.iter().map(|bytes| bytes.len() as u64).sum::<u64>() + 64;
        journal.agent_bytes.insert("reviewer".into(), cap);
        let (capture, _guard) = crate::tests::capture_spans();
        append(&journal, &stem, now, GRANT, &"x".repeat(cap as usize));
        let failures = capture.events_where(
            "dekopon_gatewayd",
            &[
                ("event", "gateway_memory_retention_failed"),
                ("reason", "agent-byte-cap"),
            ],
        );
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(
            [
                fs::read(journal.path(&stem)).unwrap(),
                fs::read(journal.path(&other)).unwrap()
            ],
            before
        );
    }

    #[test]
    fn memory_tools_offer_only_retained_routines_like_the_projection_label() {
        use dekopon_agent::memory_tools::MemoryTools;
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let mut policy = decider_policy();
        append_represented(&journal, &stem, &policy, "first", &[(1, "summary text")]);
        policy.retain.retain(|id| id != "summary");
        let tools = JournalMemoryTools::new(&journal, stem, GRANT.into(), &policy);
        assert_eq!(
            tools.read(1, "summary", None),
            "representation unavailable: missing, discarded or version mismatch; eligible representation IDs: original"
        );
    }

    fn decider_policy() -> ResolvedPolicy {
        use crate::config::memory::{Routine, RoutineKind, SelectionMode};
        let mut policy = ResolvedPolicy::default();
        for id in ["summary", "brief"] {
            policy.routines.push(Routine {
                id: id.into(),
                kind: RoutineKind::Prefix,
                max_bytes: std::num::NonZeroU64::new(1024),
                model: None,
                parts: None,
            });
            policy.retain.push(id.into());
        }
        policy.selection.mode = SelectionMode::Decider;
        policy.selection.routine = None;
        policy.selection.decision_capability = Some("decide.choose".into());
        policy.selection.fallback = Some("summary".into());
        policy
    }

    fn append_represented(
        journal: &Journal,
        stem: &str,
        policy: &ResolvedPolicy,
        user: &str,
        replacements: &[(usize, &str)],
    ) {
        let turn = ConversationTurn::completed(user, "ok");
        let version = |index: usize| crate::memory::generation::version(&policy.routines[index]);
        let mut original = Representation::original(&format!("{user}\nok"), &version(0));
        original.text = None;
        let mut representations = vec![original];
        let mut availability = vec!["original".to_owned()];
        for (index, text) in replacements {
            let id = policy.routines[*index].id.clone();
            representations.push(Representation {
                routine_id: id.clone(),
                version: version(*index),
                source_record_id: 0,
                part: None,
                kind: "prefix".into(),
                captured_bytes: text.len() as u64,
                text: Some((*text).to_owned()),
            });
            availability.push(id);
        }
        journal
            .append(
                stem,
                &Entry {
                    at: SystemTime::now(),
                    grant: GRANT,
                    turn: &turn,
                    inventory: &[],
                    next_asset_id: 1,
                    prepared: Some(&crate::memory::generation::Prepared {
                        include_chat: true,
                        representations,
                        exchanges: Vec::new(),
                        original: true,
                        availability,
                        failed_routines: Vec::new(),
                    }),
                },
                window(),
            )
            .expect("append");
    }

    #[test]
    fn a_per_record_decider_fallback_is_sent_only_when_it_saves_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Journal::open(dir.path(), RETENTION, RETENTION).expect("open");
        let stem = key().journal_stem();
        let policy = decider_policy();
        append_represented(&journal, &stem, &policy, "first", &[(1, "s"), (2, "b")]);
        append_represented(&journal, &stem, &policy, "second", &[(1, &"x".repeat(200))]);
        append_represented(&journal, &stem, &policy, &"third".repeat(40), &[(1, "s")]);
        let (capture, _guard) = crate::tests::capture_spans();
        let projected = journal
            .project(
                Projection {
                    stem: &stem,
                    grant: GRANT,
                    window: window(),
                    sealed_at: None,
                    policy: &policy,
                },
                &History::new(window().limits),
                |_available, _evidence| Ok("brief".to_owned()),
            )
            .expect("projection");
        assert_eq!(projected.history.turns().len(), 3);
        let selection = |record: &str| {
            let events = capture.events_where(
                "dekopon_gatewayd",
                &[("event", "gateway_memory_selection"), ("record_id", record)],
            );
            assert_eq!(events.len(), 1, "{events:?}");
            (
                events[0].field("requested").map(str::to_owned),
                events[0].field("actual").map(str::to_owned),
                events[0].field("reason").map(str::to_owned),
            )
        };
        let expect = |requested: &str, actual: &str, reason: &str| {
            (
                Some(requested.to_owned()),
                Some(actual.to_owned()),
                Some(reason.to_owned()),
            )
        };
        assert_eq!(selection("1"), expect("brief", "brief", "selected"));
        assert_eq!(
            selection("2"),
            expect("brief", "original", "kept-original: no-saving")
        );
        assert_eq!(
            selection("3"),
            expect("brief", "summary", "fallback: choice-unavailable")
        );
    }
}
