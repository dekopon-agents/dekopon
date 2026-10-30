pub(crate) mod records;

pub(crate) mod tools;
pub(crate) use tools::JournalMemoryTools;

use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    num::{NonZeroU64, NonZeroUsize},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dekopon_agent::memory::{CapturedTurn, Item};
use dekopon_core::{AgentId, AssetNumber, asset::AssetKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use self::records::{Boot, Snapshot, admits, walk};
use super::record::{
    ArrivedAsset, AssetRow, GrantDigest, RecordId, Slot, SummaryPlan, TurnId, TurnRecord,
};
use crate::{
    asset::{AssetRef, AssetSourceRef, PendingAsset, RecalledAsset},
    config::{MemoryWindow, memory::AttachmentSummaries},
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

/// Only an unlocked reader that met a malformed line constructs one, so a reset is always one a
/// reader asked for.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct CorruptStem(String);

#[derive(Debug)]
pub(crate) enum RecallFailure {
    Io,
    Corrupt(CorruptStem),
}

impl RecallFailure {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Corrupt(_) => "corrupt",
        }
    }
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
    pub snapshot: Option<Snapshot>,
    pub assets: Vec<RecalledAsset>,
    pub next_asset_id: u64,
    pub grant_boundary: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Keep {
    Everything,
    ExchangesOnly,
}

pub(crate) struct Entry<'a> {
    pub at: SystemTime,
    pub grant: &'a GrantDigest,
    pub captured: &'a CapturedTurn,
    pub keep: Keep,
    pub plan: Option<SummaryPlan>,
    pub attachments: AttachmentSummaries,
    pub arrivals: &'a [Vec<u64>],
    pub inventory: &'a [AssetRef],
    pub next_asset_id: u64,
}

// Writers hold `lock`; readers open their own descriptor and never take it, because every rewrite
// renames a new file over the old one and so cannot move bytes under a held inode.
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
        Self::open_at(dir, retention, seal_retention, SystemTime::now())
    }

    #[cfg(test)]
    pub(crate) fn open_at(
        dir: &Path,
        retention: Duration,
        seal_retention: Duration,
        now: SystemTime,
    ) -> Result<Self, JournalError> {
        Self::boot(
            dir,
            retention,
            retention,
            seal_retention,
            NonZeroUsize::new(64).expect("positive queue"),
            now,
        )
        .map(|(journal, _boot)| journal)
    }

    pub fn boot(
        dir: &Path,
        max_age: Duration,
        retention: Duration,
        seal_retention: Duration,
        queue: NonZeroUsize,
        now: SystemTime,
    ) -> Result<(Self, Boot), JournalError> {
        let journal = Self::create(dir, max_age, retention, seal_retention)?;
        journal.reset_legacy_files()?;
        let boot = journal.load_summary_tasks(queue)?;
        journal.expire(now)?;
        Ok((journal, boot))
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
                .map_err(blob_error)?;
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
                let Some(snapshot) = self.record_snapshot(&stem, true)? else {
                    continue;
                };
                for head in snapshot.records {
                    let grant = head.header.grant.hex();
                    for asset in head.header.assets {
                        references.insert(crate::asset::StoredKey::new(
                            stem.clone(),
                            asset.id,
                            Duration::from_millis(asset.at_ms),
                            grant.clone(),
                        ));
                    }
                }
            }
        }
        assets.lock().reconcile(&references).map_err(blob_error)
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
        file.sync_all()?;
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
        grant: &GrantDigest,
        window: MemoryWindow,
        now: SystemTime,
    ) -> Result<Recalled, RecallFailure> {
        let failed = |error: JournalError| match error {
            JournalError::Io { .. } => RecallFailure::Io,
            JournalError::Corrupt => RecallFailure::Corrupt(CorruptStem(stem.to_owned())),
        };
        let seal_ms = self
            .read_seal(stem, window.forget_after)
            .map_err(failed)?
            .map(|seal| millis(seal.at));
        let now_ms = millis(now);
        let horizon_ms = now_ms.saturating_sub(duration_millis(window.forget_after));
        let age_horizon_ms = now_ms.saturating_sub(duration_millis(self.age_for(stem)));
        let media_horizon_ms = now_ms.saturating_sub(duration_millis(WHATSAPP_MEDIA_LIFETIME));
        // One record past twice the window lets projection see the count that forces a repack.
        let limit = NonZeroUsize::new(window.limits.max_turns.saturating_mul(2).saturating_add(1))
            .unwrap_or(NonZeroUsize::MIN);
        let mut snapshot = match File::open(self.path(stem)) {
            Ok(file) => walk::window(file, limit).map_err(failed)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Recalled {
                    snapshot: None,
                    assets: Vec::new(),
                    next_asset_id: 1,
                    grant_boundary: false,
                });
            }
            Err(error) => return Err(failed(error.into())),
        };
        let admitted = snapshot
            .records
            .iter()
            .rev()
            .take_while(|head| admits(&head.header, grant, horizon_ms.max(age_horizon_ms), seal_ms))
            .count();
        let grant_boundary = snapshot
            .records
            .len()
            .checked_sub(admitted + 1)
            .is_some_and(|index| snapshot.records[index].header.grant != *grant);
        snapshot.records.drain(..snapshot.records.len() - admitted);
        let (assets, next_asset_id) = snapshot.records.last().map_or((Vec::new(), 1), |newest| {
            let assets = newest
                .header
                .assets
                .iter()
                .filter(|asset| asset.at_ms >= horizon_ms)
                .map(|asset| RecalledAsset {
                    id: asset.id,
                    arrived: UNIX_EPOCH + Duration::from_millis(asset.at_ms),
                    asset: PendingAsset {
                        name: asset.name.clone(),
                        mime: asset.mime.clone(),
                        size: asset.size,
                        source: match &asset.source {
                            Some(AssetSourceRef::WhatsApp { .. })
                                if asset.at_ms < media_horizon_ms =>
                            {
                                None
                            }
                            source => source.clone(),
                        },
                    },
                })
                .collect();
            (assets, newest.header.next_asset_id)
        });
        Ok(Recalled {
            snapshot: Some(snapshot),
            assets,
            next_asset_id,
            grant_boundary,
        })
    }

    pub(crate) fn reset(&self, corrupt: CorruptStem) {
        let _lock = self.lock.lock();
        let _assets = AssetSync(self);
        remove(&self.path(&corrupt.0));
        tracing::warn!(
            event = "gateway_journal_reset",
            reason = "corrupt",
            "gateway.journal.reset"
        );
    }

    /// Builds the record from the captured turn and writes it; ids, the turn id, slots and assets
    /// exist only here, under the lock.
    pub fn append(&self, stem: &str, entry: &Entry<'_>) -> Result<Option<RecordId>, JournalError> {
        let _lock = self.lock.lock();
        let _assets = AssetSync(self);
        let path = self.path(stem);
        let next_record_id = match File::open(&path) {
            Ok(file) => match walk::tail(&file) {
                Ok(walk::Tail::Clean) => match walk::window(file, NonZeroUsize::MIN) {
                    Ok(snapshot) => snapshot
                        .records
                        .last()
                        .map_or(NonZeroU64::MIN, |head| head.header.next_record_id),
                    Err(JournalError::Corrupt) => reset_file(&path),
                    Err(error) => return Err(error),
                },
                Ok(walk::Tail::Torn) | Err(JournalError::Corrupt) => reset_file(&path),
                Err(error) => return Err(error),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => NonZeroU64::MIN,
            Err(error) => return Err(error.into()),
        };
        let inbound: HashMap<_, _> = entry
            .inventory
            .iter()
            .filter(|asset| asset.is_inbound())
            .map(|asset| (asset.id, asset))
            .collect();
        let mut registrations = entry.arrivals.iter();
        let mut original = Vec::new();
        for item in &entry.captured.original {
            let arrivals = match item {
                Item::User(_) | Item::Steer(_) => registrations.next(),
                Item::Assistant(_) | Item::Reasoning(_) | Item::Exchange(_) | Item::Asset(_) => {
                    None
                }
            };
            let kept = match entry.keep {
                Keep::Everything => true,
                Keep::ExchangesOnly => match item {
                    Item::Exchange(_) => true,
                    Item::User(_)
                    | Item::Steer(_)
                    | Item::Assistant(_)
                    | Item::Reasoning(_)
                    | Item::Asset(_) => false,
                },
            };
            if !kept {
                continue;
            }
            original.push(item.clone());
            for id in arrivals
                .into_iter()
                .flatten()
                .filter(|id| inbound.contains_key(id))
            {
                let number =
                    AssetNumber::try_from(*id).map_err(|_invalid| JournalError::Corrupt)?;
                original.push(Item::Asset(number));
            }
        }
        let plan = match entry.keep {
            Keep::Everything => entry.plan.clone(),
            Keep::ExchangesOnly => None,
        };
        let record = TurnRecord {
            id: RecordId(next_record_id),
            turn: TurnId(mint_turn_id()),
            at_ms: millis(entry.at),
            grant: entry.grant.clone(),
            model: entry.captured.model.clone(),
            original,
            summaries: plan
                .as_ref()
                .map_or_else(Vec::new, |plan| vec![Slot::Queued; plan.parts.len()]),
            plan,
            assets: entry
                .inventory
                .iter()
                .filter(|asset| !matches!(&asset.source, Some(AssetSourceRef::Generated { .. })))
                .map(|asset| AssetRow {
                    id: asset.id,
                    at_ms: millis(asset.arrived),
                    name: asset.name.clone(),
                    mime: asset.mime.clone(),
                    size: asset.size,
                    source: asset.source.clone(),
                })
                .collect(),
            next_asset_id: entry.next_asset_id,
        };
        let mut arrived = Vec::new();
        for (item, original) in record.original.iter().enumerate() {
            let number = match original {
                Item::Asset(number) => *number,
                Item::User(_)
                | Item::Steer(_)
                | Item::Assistant(_)
                | Item::Reasoning(_)
                | Item::Exchange(_) => continue,
            };
            let Some(asset) = inbound.get(&u64::from(number)) else {
                continue;
            };
            let kind = AssetKind::of(&asset.mime);
            arrived.push(ArrivedAsset {
                item: u32::try_from(item).map_err(|_invalid| JournalError::Corrupt)?,
                number,
                kind,
                bytes: asset.size,
                summaries: record
                    .plan
                    .as_ref()
                    .filter(|_| entry.attachments.summarizes(kind))
                    .map_or_else(Vec::new, |plan| vec![Slot::Queued; plan.parts.len()]),
            });
        }
        let mut size = records::Counted(0);
        records::encode_record(&mut size, &record, arrived.clone())?;
        let agent = stem.split_once('/').map(|(agent, _)| agent);
        if agent.is_some_and(|agent| size.0 > self.agent_cap(agent)) {
            tracing::warn!(
                event = "gateway_memory_retention_failed",
                reason = "agent-byte-cap",
                encoded_bytes = size.0,
                "gateway.memory.retention.failed"
            );
            return Ok(None);
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
        let mut buffered = io::BufWriter::with_capacity(64 * 1024, &mut file);
        records::encode_record(&mut buffered, &record, arrived.clone())?;
        buffered.flush()?;
        drop(buffered);
        // Ownership transfers before fallible scans can encounter another corrupt journal file.
        if let Some(assets) = self
            .assets
            .lock()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        {
            let grant = record.grant.hex();
            assets
                .lock()
                .retain_record(record.assets.iter().map(|asset| {
                    crate::asset::StoredKey::new(
                        stem.to_owned(),
                        asset.id,
                        Duration::from_millis(asset.at_ms),
                        grant.clone(),
                    )
                }));
        }
        file.sync_all()?;
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        File::open(&self.dir)?.sync_all()?;
        tracing::info!(
            event = "gateway_memory_published",
            memory.record.id = record.id.0.get(),
            memory.generation = record.turn.0,
            memory.record.bytes = size.0,
            memory.record.items = record.original.len(),
            memory.slots.queued = record.summaries.len()
                + arrived
                    .iter()
                    .map(|asset| asset.summaries.len())
                    .sum::<usize>(),
            memory.assets.arrived = arrived.len(),
            "gateway.memory.published"
        );
        Ok(Some(record.id))
    }

    /// The directory sweep, this stem's horizon and its agent's byte cap: every rewrite here
    /// writes a new file and renames it, so readers holding the old inode keep their bytes.
    pub(crate) fn maintain(&self, stem: &str, now: SystemTime, next_asset_id: u64) {
        let _lock = self.lock.lock();
        let _assets = AssetSync(self);
        let maintained = (|| -> Result<(), JournalError> {
            // File mtimes are wall-clock, so the directory sweep never runs on a caller's clock.
            self.sweep(SystemTime::now(), false)?;
            let path = self.path(stem);
            let horizon = millis(now).saturating_sub(duration_millis(self.age_for(stem)));
            match sweep_file(&path, horizon) {
                Ok(evicted) => expired(&evicted, Expiry::RecordAge),
                Err(JournalError::Corrupt) => {
                    remove(&path);
                    tracing::warn!(
                        event = "gateway_journal_reset",
                        reason = "corrupt",
                        "gateway.journal.reset"
                    );
                }
                Err(error) => return Err(error),
            }
            if let Some((agent, _)) = stem.split_once('/') {
                self.enforce_agent_cap(agent)?;
            }
            self.finish_pending_assets(stem, next_asset_id)
        })();
        if let Err(error) = maintained {
            tracing::warn!(reason = error.label(), %error, "gateway.journal.maintain.failed");
        }
    }

    pub(crate) fn enforce_caps(&self) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        let _assets = AssetSync(self);
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if is_agent_directory(&path)
                && let Some(agent) = path.file_name().and_then(|name| name.to_str())
            {
                self.enforce_agent_cap(agent)?;
            }
        }
        Ok(())
    }

    fn enforce_agent_cap(&self, agent: &str) -> Result<(), JournalError> {
        let limit = self.agent_cap(agent);
        let directory = self.dir.join(agent);
        if !directory.exists() {
            return Ok(());
        }
        let mut files = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if is_journal_file(&path) {
                let size = fs::metadata(&path)?.len();
                total = total.saturating_add(size);
                files.push((path, size));
            }
        }
        if total <= limit {
            return Ok(());
        }
        let mut heads = Vec::new();
        for (index, (path, size)) in files.iter().enumerate() {
            let snapshot = match records::scan_path(path) {
                Ok(snapshot) => snapshot,
                Err(JournalError::Corrupt) => {
                    remove(path);
                    total = total.saturating_sub(*size);
                    continue;
                }
                Err(error) => return Err(error),
            };
            for head in snapshot.records {
                let cost = head
                    .header
                    .body_len
                    .saturating_add(serde_json::to_vec(&head.header).map_or(0, |h| h.len() as u64))
                    .saturating_add(7);
                heads.push((head.header.at_ms, index, head.header.id, cost));
            }
        }
        heads.sort_by(|left, right| (left.0, &files[left.1].0).cmp(&(right.0, &files[right.1].0)));
        let mut cut: BTreeMap<usize, RecordId> = BTreeMap::new();
        for (_, index, id, cost) in heads {
            if total <= limit {
                break;
            }
            total = total.saturating_sub(cost);
            cut.insert(index, id);
        }
        for (index, last_cut) in cut {
            let path = &files[index].0;
            let mut evicted = Vec::new();
            records::rewrite_path(path, |header| {
                let kept = header.id > last_cut;
                if !kept {
                    evicted.push(header.id);
                }
                kept
            })?;
            expired(&evicted, Expiry::AgentCap);
            if fs::metadata(path).is_ok_and(|metadata| metadata.len() == 0) {
                remove(path);
            }
        }
        Ok(())
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

    pub(crate) fn age_horizon_ms(&self, stem: &str, now: SystemTime) -> u64 {
        millis(now).saturating_sub(duration_millis(self.age_for(stem)))
    }

    fn age_for(&self, stem: &str) -> Duration {
        stem.split_once('/')
            .and_then(|(agent, _)| self.agent_ages.get(agent))
            .copied()
            .unwrap_or(self.max_age)
    }

    fn reset_legacy_files(&self) -> Result<(), JournalError> {
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if is_journal_file(&path) && !path.with_extension(FOLLOW_EXTENSION).exists() {
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
        let _lock = self.lock.lock();
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
                    let file_path = file.path();
                    if !is_journal_file(&file_path) {
                        continue;
                    }
                    if cutoff.is_some_and(|limit| {
                        file.metadata()
                            .and_then(|m| m.modified())
                            .is_ok_and(|modified| modified < limit)
                    }) {
                        let evicted: Vec<RecordId> = records::scan_path(&file_path)
                            .map(|snapshot| {
                                snapshot.records.iter().map(|head| head.header.id).collect()
                            })
                            .unwrap_or_default();
                        if remove(&file_path) {
                            expired(&evicted, Expiry::FileAge);
                        }
                    } else if records {
                        let age = self.age_for(&format!(
                            "{}/{}",
                            path.file_name().unwrap_or_default().to_string_lossy(),
                            file_path.file_stem().unwrap_or_default().to_string_lossy()
                        ));
                        let horizon = millis(now).saturating_sub(duration_millis(age));
                        match sweep_file(&file_path, horizon) {
                            Ok(evicted) => expired(&evicted, Expiry::RecordAge),
                            Err(JournalError::Corrupt) => {
                                remove(&file_path);
                                tracing::warn!(
                                    event = "gateway_journal_reset",
                                    reason = "corrupt",
                                    "gateway.journal.reset"
                                );
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
                continue;
            }
            let stale = match path.extension() {
                Some(extension) if extension == SEAL_EXTENSION => {
                    let modified = entry.metadata()?.modified()?;
                    if seal_cutoff.is_some_and(|limit| modified < limit) {
                        Some(Expiry::Seal)
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
                                None
                            }
                            Err(JournalError::Io {
                                kind: io::ErrorKind::NotFound,
                            }) => None,
                            Err(error) => return Err(error),
                            Ok(_) => None,
                        }
                    }
                }
                Some(extension) if extension == FOLLOW_EXTENSION => read_follow_marker(&path)?
                    .is_some_and(|marker| marker.until_ms <= millis(now))
                    .then_some(Expiry::Follow),
                _ => None,
            };
            if let Some(reason) = stale
                && remove(&path)
            {
                let follow = match reason {
                    Expiry::Follow => true,
                    Expiry::FileAge | Expiry::RecordAge | Expiry::AgentCap | Expiry::Seal => false,
                };
                if records && follow {
                    tracing::info!(
                        audit.event = "gateway.follow",
                        operation = "banished",
                        cause = "lapse",
                        "gateway.follow"
                    );
                }
                expired(&[], reason);
            }
        }
        Ok(())
    }
}

fn blob_error(error: dekopon_model::asset::BlobError) -> JournalError {
    match error {
        dekopon_model::asset::BlobError::Io(kind) => JournalError::Io { kind },
        dekopon_model::asset::BlobError::TooLarge
        | dekopon_model::asset::BlobError::Disabled
        | dekopon_model::asset::BlobError::Capacity
        | dekopon_model::asset::BlobError::Unknown
        | dekopon_model::asset::BlobError::Reclaimed
        | dekopon_model::asset::BlobError::Unauthorized
        | dekopon_model::asset::BlobError::LengthChanged => JournalError::Corrupt,
    }
}

fn reset_file(path: &Path) -> NonZeroU64 {
    remove(path);
    tracing::warn!(
        event = "gateway_journal_reset",
        reason = "corrupt",
        "gateway.journal.reset"
    );
    NonZeroU64::MIN
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

pub(crate) fn millis(at: SystemTime) -> u64 {
    duration_millis(at.duration_since(UNIX_EPOCH).unwrap_or_default())
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn sweep_file(path: &Path, horizon: u64) -> Result<Vec<RecordId>, JournalError> {
    let mut evicted = Vec::new();
    if records::first_header(path)?.is_none_or(|header| header.at_ms >= horizon) {
        return Ok(evicted);
    }
    records::rewrite_path(path, |header| {
        let kept = header.at_ms >= horizon;
        if !kept {
            evicted.push(header.id);
        }
        kept
    })?;
    if fs::metadata(path)?.len() == 0 {
        remove(path);
    }
    Ok(evicted)
}

#[derive(Clone, Copy)]
enum Expiry {
    FileAge,
    RecordAge,
    AgentCap,
    Seal,
    Follow,
}

fn expired(evicted: &[RecordId], reason: Expiry) {
    let whole_file = match reason {
        Expiry::FileAge | Expiry::Seal | Expiry::Follow => true,
        Expiry::RecordAge | Expiry::AgentCap => false,
    };
    if evicted.is_empty() && !whole_file {
        return;
    }
    let ids: Vec<String> = evicted.iter().map(|id| id.0.to_string()).collect();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    dekopon_telemetry::with_event_string_array("memory.record.ids", &ids, || {
        tracing::info!(
            event = "gateway_journal_expired",
            memory.records.evicted = evicted.len(),
            memory.expiry.reason = match reason {
                Expiry::FileAge => "file-age",
                Expiry::RecordAge => "record-age",
                Expiry::AgentCap => "agent-cap",
                Expiry::Seal => "seal",
                Expiry::Follow => "follow",
            },
            "gateway.journal.expired"
        );
    });
}

#[cfg(test)]
impl Journal {
    pub(crate) fn hold_io_for_test(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.lock.lock()
    }
}

#[cfg(test)]
mod tests;
