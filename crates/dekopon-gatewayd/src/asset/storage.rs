use super::*;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) enum Refill {
    Platform,
    None,
}

impl Refill {
    pub(super) fn of(source: &AssetSourceRef) -> Self {
        match source {
            AssetSourceRef::Fauxlack { .. }
            | AssetSourceRef::Slack { .. }
            | AssetSourceRef::Discord { .. }
            | AssetSourceRef::Telegram { .. }
            | AssetSourceRef::WhatsApp { .. } => Self::Platform,
            AssetSourceRef::Generated { .. } => Self::None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum RefetchOutcome {
    Refetched,
    PlatformGone,
    Revoked,
    Unavailable,
}
impl RefetchOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Refetched => "refetched",
            Self::PlatformGone => "platform-gone",
            Self::Revoked => "revoked",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(super) enum StoreFailure {
    BudgetTooSmall,
    StoredTooLarge,
    DecodedTooLarge,
    AllPinned,
    Io,
}

impl StoreFailure {
    fn label(self) -> &'static str {
        match self {
            Self::BudgetTooSmall => "budget-too-small",
            Self::StoredTooLarge => "stored-too-large",
            Self::DecodedTooLarge => "decoded-too-large",
            Self::AllPinned => "all-pinned",
            Self::Io => "io",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct StoredKey {
    stem: String,
    id: u64,
    at_ms: u64,
    grant: String,
}

impl StoredKey {
    pub(crate) fn new(stem: String, id: u64, arrived: Duration, grant: String) -> Self {
        Self {
            stem,
            id,
            at_ms: arrived.as_millis() as u64,
            grant,
        }
    }
    fn of(key: &AssetStateKey, asset: &AssetRef) -> Self {
        Self::new(
            key.conversation.journal_stem(),
            asset.id,
            asset.arrived.duration_since(UNIX_EPOCH).unwrap_or_default(),
            key.grant.as_ref().clone(),
        )
    }
}

#[derive(Deserialize, Serialize)]
struct Binding {
    key: StoredKey,
    digest: Option<String>,
    failure: Option<StoreFailure>,
    class: Refill,
    retained: bool,
}

#[derive(Deserialize, Serialize)]
struct StoredFile {
    len: usize,
    used: u64,
    resident: bool,
    #[serde(skip)]
    data: Option<DiskBlob>,
}

#[derive(Deserialize, Serialize)]
struct Index {
    version: u8,
    clock: u64,
    bindings: Vec<Binding>,
    files: HashMap<String, StoredFile>,
}

struct IndexDigest(Sha256);

impl Write for IndexDigest {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct Retention {
    pub(super) budget: usize,
    pub(super) bytes: usize,
    pub(super) released: VecDeque<RetentionKey>,
    dir: Option<PathBuf>,
    index: Index,
    persisted: Option<[u8; 32]>,
    references: HashSet<StoredKey>,
    orphans: Vec<DiskBlob>,
    starting: bool,
    publications: Vec<Arc<PublicationRange>>,
    #[cfg(test)]
    pub(super) after_publish_error: Option<io::ErrorKind>,
}

impl Retention {
    pub(super) fn new(budget: usize) -> Self {
        Self {
            budget,
            bytes: 0,
            released: VecDeque::new(),
            dir: None,
            persisted: None,
            references: HashSet::new(),
            orphans: Vec::new(),
            starting: false,
            publications: Vec::new(),
            #[cfg(test)]
            after_publish_error: None,
            index: Index {
                version: 2,
                clock: 0,
                bindings: Vec::new(),
                files: HashMap::new(),
            },
        }
    }

    pub(super) fn open(dir: &Path, budget: usize) -> Result<Self, BlobError> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir.join("blobs"))?;
        for directory in [dir.to_owned(), dir.join("blobs")] {
            let metadata = fs::symlink_metadata(&directory)?;
            if !metadata.is_dir() || metadata.uid() != crate::current_uid() {
                return Err(BlobError::Io(io::ErrorKind::PermissionDenied));
            }
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        let mut store = Self::new(budget);
        store.dir = Some(dir.to_owned());
        store.starting = true;
        let mut loaded = false;
        store.index = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("index.json"))
        {
            Ok(file) => {
                loaded = true;
                if file.metadata()?.uid() != crate::current_uid() {
                    return Err(BlobError::Io(io::ErrorKind::PermissionDenied));
                }
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
                serde_json::from_reader(file).map_err(|error: serde_json::Error| {
                    BlobError::Io(error.io_error_kind().unwrap_or(io::ErrorKind::InvalidData))
                })?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => store.index,
            Err(error) => return Err(error.into()),
        };
        if store.index.version != 2 {
            return Err(BlobError::Io(io::ErrorKind::InvalidData));
        }
        if loaded {
            store.persisted = Some(store.index_digest()?);
        }
        for (digest, file) in &mut store.index.files {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(BlobError::Io(io::ErrorKind::InvalidData));
            }
            match fs::metadata(dir.join("blobs").join(digest)) {
                Ok(metadata)
                    if metadata.is_file() && metadata.len() == file.len as u64 && file.resident =>
                {
                    store.bytes = store
                        .bytes
                        .checked_add(file.len.max(1))
                        .ok_or(BlobError::TooLarge)?;
                }
                Ok(_) => file.resident = false,
                Err(error) if error.kind() == io::ErrorKind::NotFound => file.resident = false,
                Err(error) => return Err(error.into()),
            }
        }
        match fs::remove_file(dir.join("index.pending")) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        store.persist()?;
        Ok(store)
    }

    pub(super) fn report_startup(&self) -> Result<(), BlobError> {
        if let Some(dir) = &self.dir {
            let stats = rustix::fs::statvfs(dir).map_err(io::Error::from)?;
            tracing::info!(target: "dekopon_gatewayd::audit", { audit.event = "gateway.asset.store_started",
                asset.store.path = %dir.display(), asset.store.free_bytes = stats.f_bavail.saturating_mul(stats.f_frsize),
                asset.store.budget_bytes = self.budget }, "gateway.asset.store_started");
        }
        Ok(())
    }

    fn index_digest(&self) -> Result<[u8; 32], BlobError> {
        let mut digest = IndexDigest(Sha256::new());
        serde_json::to_writer(&mut digest, &self.index).map_err(|error| {
            BlobError::Io(error.io_error_kind().unwrap_or(io::ErrorKind::InvalidData))
        })?;
        Ok(digest.0.finalize().into())
    }

    fn persist(&mut self) -> Result<(), BlobError> {
        if let Some(dir) = &self.dir {
            let digest = self.index_digest()?;
            if self.persisted == Some(digest) {
                return Ok(());
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .custom_flags(libc::O_NOFOLLOW)
                .mode(0o600)
                .open(dir.join("index.pending"))?;
            if file.metadata()?.uid() != crate::current_uid() {
                return Err(BlobError::Io(io::ErrorKind::PermissionDenied));
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            serde_json::to_writer(&mut file, &self.index).map_err(|error| {
                BlobError::Io(error.io_error_kind().unwrap_or(io::ErrorKind::InvalidData))
            })?;
            file.sync_all()?;
            fs::rename(dir.join("index.pending"), dir.join("index.json"))?;
            File::open(dir)?.sync_all()?;
            self.persisted = Some(digest);
        }
        Ok(())
    }

    pub(super) fn miss(&self, id: u64, bytes: usize, reason: &'static str) {
        tracing::info!(target: "dekopon_gatewayd::audit", { audit.event = "gateway.asset.retention_miss",
            asset.id = id, asset.bytes = bytes, asset.budget = self.budget, reason }, "gateway.asset.retention_miss");
    }

    pub(super) fn check_size(&self, id: u64, bytes: usize) -> Result<(), BlobError> {
        if self.budget == 0 {
            self.miss(id, bytes, "disabled");
            return Err(BlobError::Disabled);
        }
        if bytes.max(1) > self.budget || bytes > dekopon_model::asset::MAX_STORED_ATTACHMENT_BYTES {
            self.miss(id, bytes, "oversized");
            return Err(BlobError::TooLarge);
        }
        Ok(())
    }

    pub(super) fn failure(&self, len: usize, error: BlobError) -> StoreFailure {
        match error {
            BlobError::TooLarge if len.max(1) > self.budget => StoreFailure::BudgetTooSmall,
            BlobError::TooLarge if len > dekopon_model::asset::MAX_STORED_ATTACHMENT_BYTES => {
                StoreFailure::StoredTooLarge
            }
            BlobError::TooLarge => StoreFailure::DecodedTooLarge,
            BlobError::Capacity => StoreFailure::AllPinned,
            BlobError::Io(_) => StoreFailure::Io,
            BlobError::LengthChanged => StoreFailure::Io,
            BlobError::Disabled
            | BlobError::Unknown
            | BlobError::Reclaimed
            | BlobError::Unauthorized => StoreFailure::Io,
        }
    }

    pub(super) fn lookup(&self, key: &AssetStateKey, asset: &AssetRef) -> Attachment {
        let key = StoredKey::of(key, asset);
        let Some(binding) = self
            .index
            .bindings
            .iter()
            .find(|binding| binding.key == key)
        else {
            return Attachment::Registered;
        };
        match (&binding.digest, binding.failure) {
            (_, Some(reason)) => Attachment::NotStored(reason),
            (Some(digest), None) if self.resident(digest) => Attachment::Resident(digest.clone()),
            (Some(_), None) => Attachment::Evicted,
            (None, None) => Attachment::Registered,
        }
    }

    pub(super) fn record_failure(
        &mut self,
        key: &AssetStateKey,
        asset: &AssetRef,
        error: BlobError,
    ) -> Attachment {
        if error == BlobError::Disabled {
            return Attachment::Registered;
        }
        match error {
            BlobError::Io(_)
            | BlobError::LengthChanged
            | BlobError::Unknown
            | BlobError::Reclaimed
            | BlobError::Unauthorized => {
                tracing::info!(target: "dekopon_gatewayd::audit", { audit.event = "gateway.asset.retention_miss",
                    asset.id = asset.id, asset.bytes = asset.size.unwrap_or_default(), asset.budget = self.budget,
                    reason = "storage", error = %error }, "gateway.asset.retention_miss");
            }
            BlobError::TooLarge
                if asset.size.unwrap_or_default() <= self.budget as u64
                    && asset.size.unwrap_or_default()
                        <= dekopon_model::asset::MAX_STORED_ATTACHMENT_BYTES as u64 =>
            {
                self.miss(
                    asset.id,
                    asset.size.unwrap_or_default() as usize,
                    "oversized",
                );
            }
            BlobError::TooLarge | BlobError::Disabled | BlobError::Capacity => (),
        }
        let reason = self.failure(asset.size.unwrap_or_default() as usize, error);
        tracing::info!(target: "dekopon_gatewayd::audit", { audit.event = "gateway.asset.store_failed",
            reason = reason.label(), asset.bytes = asset.size.unwrap_or_default(),
            asset.store.bytes = self.bytes, asset.store.budget_bytes = self.budget }, "gateway.asset.store_failed");
        let key = StoredKey::of(key, asset);
        self.index.bindings.retain(|binding| binding.key != key);
        self.index.bindings.push(Binding {
            retained: self.references.contains(&key),
            key,
            digest: None,
            failure: Some(reason),
            class: asset.source.as_ref().map_or(Refill::Platform, Refill::of),
        });
        if let Err(error) = self.persist() {
            tracing::warn!(%error, "gateway.asset.reclaim.failed");
        }
        Attachment::NotStored(reason)
    }

    pub(super) fn is_bound(&self, key: &AssetStateKey, asset: &AssetRef, digest: &str) -> bool {
        let key = StoredKey::of(key, asset);
        self.resident(digest)
            && self
                .index
                .bindings
                .iter()
                .any(|binding| binding.key == key && binding.digest.as_deref() == Some(digest))
    }

    pub(super) fn pin(&mut self, digest: &str, recency: Recency) -> Result<DiskBlob, BlobError> {
        let file = self
            .index
            .files
            .get_mut(digest)
            .ok_or(BlobError::Reclaimed)?;
        if !file.resident {
            return Err(BlobError::Reclaimed);
        }
        if file.data.is_none() {
            let dir = self.dir.as_ref().ok_or(BlobError::Reclaimed)?;
            file.data = Some(DiskBlob::from_descriptor(
                File::open(dir.join("blobs").join(digest))?.into(),
                file.len,
            )?);
        }
        let data = file.data.as_ref().ok_or(BlobError::Reclaimed)?.clone();
        match recency {
            Recency::Touch => {
                self.index.clock += 1;
                file.used = self.index.clock;
                self.persist()?;
            }
            Recency::Keep => (),
        }
        Ok(data)
    }

    pub(super) fn resident(&self, digest: &str) -> bool {
        self.index
            .files
            .get(digest)
            .is_some_and(|file| file.resident)
    }

    pub(super) fn refetch(&self, outcome: RefetchOutcome) {
        tracing::info!(target: "dekopon_gatewayd::audit", { audit.event = "gateway.asset.refetch",
            outcome = outcome.label(), asset.store.bytes = self.bytes,
            asset.store.budget_bytes = self.budget }, "gateway.asset.refetch");
    }

    pub(super) fn reserve(&mut self, id: u64, len: usize) -> Result<(), BlobError> {
        self.check_size(id, len)?;
        self.sweep();
        let needed = (self.bytes + len.max(1)).saturating_sub(self.budget);
        if needed == 0 {
            return Ok(());
        }
        let mut candidates: Vec<_> = self
            .index
            .files
            .iter()
            .filter_map(|(digest, file)| {
                if !file.resident || file.data.as_ref().is_some_and(DiskBlob::is_pinned) {
                    return None;
                }
                let class = self
                    .index
                    .bindings
                    .iter()
                    .filter(|binding| binding.digest.as_ref() == Some(digest))
                    .map(|binding| binding.class)
                    .max()
                    .unwrap_or(Refill::Platform);
                Some((digest.clone(), class, file.used, file.len.max(1)))
            })
            .collect();
        candidates.sort_by_key(|(_, class, used, _)| (*class, *used));
        if candidates.iter().map(|(_, _, _, len)| *len).sum::<usize>() < needed {
            self.miss(id, len, "all-pinned");
            return Err(BlobError::Capacity);
        }
        for (digest, class, _, _) in candidates {
            if self.bytes + len.max(1) <= self.budget {
                break;
            }
            let bytes = self.index.files.get(&digest).map_or(0, |file| file.len);
            self.evict_digest(&digest)?;
            let origin = match class {
                Refill::Platform => "inbound",
                Refill::None => "generated",
            };
            tracing::info!(target: "dekopon_gatewayd::audit", { audit.event = "gateway.asset.evicted",
                asset.origin = origin, asset.bytes = bytes, asset.store.bytes = self.bytes,
                asset.store.budget_bytes = self.budget }, "gateway.asset.evicted");
        }
        self.persist()
    }

    fn evict_digest(&mut self, digest: &str) -> Result<(), BlobError> {
        let file = self
            .index
            .files
            .get_mut(digest)
            .ok_or(BlobError::Reclaimed)?;
        if !file.resident {
            return Ok(());
        }
        if let Some(data) = &file.data {
            data.reclaim()?;
        }
        file.data = None;
        if let Some(dir) = &self.dir {
            match fs::remove_file(dir.join("blobs").join(digest)) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        file.data = None;
        file.resident = false;
        self.bytes -= file.len.max(1);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn index_snapshot(&self) -> serde_json::Value {
        serde_json::to_value(&self.index).unwrap()
    }

    #[cfg(test)]
    pub(super) fn resident_files(&self) -> usize {
        self.index
            .files
            .values()
            .filter(|file| file.resident)
            .count()
            + self.orphans.len()
    }

    pub(super) fn store_bytes(
        &mut self,
        key: &AssetStateKey,
        asset: &AssetRef,
        bytes: &[u8],
    ) -> Result<String, BlobError> {
        let digest: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if self.resident(&digest) {
            self.bind(key, asset, digest.clone())?;
            return Ok(digest);
        }
        self.reserve(asset.id, bytes.len())?;
        self.store(key, asset, DiskBlob::from_bytes(bytes)?)
    }

    pub(super) fn store(
        &mut self,
        key: &AssetStateKey,
        asset: &AssetRef,
        data: DiskBlob,
    ) -> Result<String, BlobError> {
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        let mut offset = 0;
        while offset < data.len() {
            let len = buffer.len().min(data.len() - offset);
            data.read_exact_at(&mut buffer[..len], offset as u64)?;
            hash.update(&buffer[..len]);
            offset += len;
        }
        let digest = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !self.resident(&digest) {
            self.reserve(asset.id, data.len())?;
            let owned = if let Some(dir) = &self.dir {
                let blobs = dir.join("blobs");
                let mut file = tempfile::NamedTempFile::new_in(&blobs)?;
                let mut offset = 0;
                while offset < data.len() {
                    let len = buffer.len().min(data.len() - offset);
                    data.read_exact_at(&mut buffer[..len], offset as u64)?;
                    file.write_all(&buffer[..len])?;
                    offset += len;
                }
                file.as_file().sync_all()?;
                file.persist(blobs.join(&digest))
                    .map_err(|error| error.error)?;
                self.charge(digest.clone(), data.len(), None);
                #[cfg(test)]
                if let Some(kind) = self.after_publish_error.take() {
                    return Err(BlobError::Io(kind));
                }
                File::open(&blobs)?.sync_all()?;
                DiskBlob::from_descriptor(File::open(blobs.join(&digest))?.into(), data.len())?
            } else {
                data
            };
            if self.dir.is_none() {
                self.charge(digest.clone(), owned.len(), Some(owned));
            } else if let Some(file) = self.index.files.get_mut(&digest) {
                file.data = Some(owned);
            }
        }
        self.bind(key, asset, digest.clone())?;
        Ok(digest)
    }

    fn charge(&mut self, digest: String, len: usize, data: Option<DiskBlob>) {
        self.index.clock += 1;
        self.bytes += len.max(1);
        self.index.files.insert(
            digest,
            StoredFile {
                len,
                used: self.index.clock,
                resident: true,
                data,
            },
        );
    }

    fn bind(
        &mut self,
        key: &AssetStateKey,
        asset: &AssetRef,
        digest: String,
    ) -> Result<(), BlobError> {
        self.index.clock += 1;
        if let Some(file) = self.index.files.get_mut(&digest) {
            file.used = self.index.clock;
        }
        let stored_key = StoredKey::of(key, asset);
        let class = asset.source.as_ref().map_or(Refill::Platform, Refill::of);
        self.index
            .bindings
            .retain(|binding| binding.key != stored_key);
        {
            self.index.bindings.push(Binding {
                key: stored_key.clone(),
                digest: Some(digest.clone()),
                failure: None,
                class,
                retained: self.references.contains(&stored_key),
            });
        }
        self.persist()
    }

    pub(super) fn tombstone(&mut self, key: RetentionKey) {
        self.released.push_back(key);
        while self.released.len() > MAX_RELEASE_TOMBSTONES {
            self.released.pop_front();
        }
    }

    #[cfg(test)]
    pub(super) fn evict(&mut self, key: RetentionKey, row: &mut Row) -> Result<(), BlobError> {
        if let Attachment::Resident(digest) = &row.state {
            self.evict_digest(digest)?;
        }
        row.state = Attachment::Evicted;
        self.tombstone(key);
        self.persist()
    }

    pub(super) fn release_all(&mut self, key: &AssetStateKey, rows: impl IntoIterator<Item = Row>) {
        for row in rows {
            self.tombstone((key.clone(), row.asset.id));
            match row.state {
                Attachment::Downloading(downloading) => downloading.task.abort(),
                Attachment::Resident(_)
                | Attachment::Registered
                | Attachment::Evicted
                | Attachment::NotStored(_)
                | Attachment::PlatformGone => (),
            }
            let stored = StoredKey::of(key, &row.asset);
            if self.dir.is_none()
                || (!self.references.contains(&stored)
                    && !self
                        .publications
                        .iter()
                        .filter(|publication| Arc::strong_count(publication) > 1)
                        .any(|publication| publication.contains(key, stored.id)))
                || row
                    .asset
                    .source
                    .as_ref()
                    .is_some_and(|source| Refill::of(source) == Refill::None)
            {
                self.index.bindings.retain(|binding| binding.key != stored);
            }
        }
        if let Err(error) = self.collect() {
            tracing::warn!(%error, "gateway.asset.reclaim.failed");
        }
    }

    fn collect(&mut self) -> Result<(), BlobError> {
        let named: HashSet<_> = self
            .index
            .bindings
            .iter()
            .filter_map(|binding| binding.digest.clone())
            .collect();
        let gone: Vec<_> = self
            .index
            .files
            .keys()
            .filter(|digest| !named.contains(*digest))
            .cloned()
            .collect();
        for digest in gone {
            let pinned = self
                .index
                .files
                .get(&digest)
                .and_then(|file| file.data.as_ref())
                .is_some_and(DiskBlob::is_pinned);
            if pinned {
                if let Some(dir) = &self.dir {
                    match fs::remove_file(dir.join("blobs").join(&digest)) {
                        Ok(()) => (),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                        Err(error) => return Err(error.into()),
                    }
                }
                if let Some(file) = self.index.files.remove(&digest)
                    && let Some(data) = file.data
                {
                    self.orphans.push(data);
                }
            } else {
                self.evict_digest(&digest)?;
                self.index.files.remove(&digest);
            }
        }
        self.persist()
    }

    pub(super) fn sweep(&mut self) {
        for data in std::mem::take(&mut self.orphans) {
            match data.reclaim() {
                Ok(()) => self.bytes -= data.len().max(1),
                Err(BlobError::Capacity) => self.orphans.push(data),
                Err(error) => {
                    tracing::warn!(%error, "gateway.asset.reclaim.failed");
                    self.orphans.push(data);
                }
            }
        }
        if let Err(error) = self.collect() {
            tracing::warn!(%error, "gateway.asset.reclaim.failed");
        }
    }

    pub(super) fn has_finished_publications(&self) -> bool {
        self.publications
            .iter()
            .any(|publication| Arc::strong_count(publication) == 1)
    }

    pub(super) fn finish_publications(&mut self) -> Result<(), BlobError> {
        while let Some(index) = self
            .publications
            .iter()
            .position(|publication| Arc::strong_count(publication) == 1)
        {
            let publication = self.publications.swap_remove(index);
            self.finish_pending(
                &publication.key.conversation.journal_stem(),
                publication.end.load(Ordering::Acquire),
            )?;
        }
        Ok(())
    }

    pub(crate) fn retain_record(&mut self, keys: impl IntoIterator<Item = StoredKey>) {
        self.references.extend(keys);
        for binding in &mut self.index.bindings {
            if self.references.contains(&binding.key) {
                binding.retained = true;
            }
        }
    }

    pub(crate) fn finish_pending(&mut self, stem: &str, next_id: u64) -> Result<(), BlobError> {
        self.index.bindings.retain(|binding| {
            binding.retained
                || binding.class == Refill::None
                || binding.key.stem != stem
                || binding.key.id >= next_id
                || self
                    .publications
                    .iter()
                    .filter(|publication| Arc::strong_count(publication) > 1)
                    .any(|publication| publication.contains_stored(&binding.key))
        });
        self.collect()
    }

    pub(crate) fn reconcile(&mut self, references: &HashSet<StoredKey>) -> Result<(), BlobError> {
        self.references.clone_from(references);
        let starting = self.starting;
        self.index.bindings.retain_mut(|binding| {
            if references.contains(&binding.key) && (!starting || binding.class == Refill::Platform)
            {
                binding.retained = true;
                true
            } else {
                !starting && !binding.retained
            }
        });
        self.collect()?;
        if starting {
            // The journal must recover committed pending bindings before crash leftovers are swept.
            if let Some(dir) = &self.dir {
                for entry in fs::read_dir(dir.join("blobs"))? {
                    let path = entry?.path();
                    let keep = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|digest| self.index.files.get(digest))
                        .is_some_and(|file| file.resident);
                    if !keep {
                        fs::remove_file(path)?;
                    }
                }
            }
            self.starting = false;
        }
        Ok(())
    }
}

struct PublicationRange {
    key: AssetStateKey,
    end: AtomicU64,
}

impl PublicationRange {
    fn contains(&self, key: &AssetStateKey, id: u64) -> bool {
        self.key == *key && id < self.end.load(Ordering::Acquire)
    }

    fn contains_stored(&self, key: &StoredKey) -> bool {
        key.stem == self.key.conversation.journal_stem()
            && key.grant == *self.key.grant
            && key.id < self.end.load(Ordering::Acquire)
    }
}

pub(crate) struct Publication {
    retention: Arc<Mutex<Retention>>,
    access: AssetAccess,
    range: Arc<PublicationRange>,
}

impl Publication {
    pub(super) fn new(retention: &Arc<Mutex<Retention>>, access: &AssetAccess) -> Option<Self> {
        if retention.lock().dir.is_none() || access.key.generation.is_none() {
            return None;
        }
        let range = Arc::new(PublicationRange {
            key: access.key.clone(),
            end: AtomicU64::new(u64::MAX),
        });
        retention.lock().publications.push(Arc::clone(&range));
        Some(Self {
            retention: Arc::clone(retention),
            access: access.clone(),
            range,
        })
    }

    pub(crate) fn finish_blocking(self) {
        let retention = Arc::clone(&self.retention);
        drop(self);
        if let Err(error) = retention.lock().finish_publications() {
            tracing::warn!(%error, "gateway.asset.reclaim.failed");
        }
    }

    pub(crate) fn seal(self) -> Self {
        self.range
            .end
            .store(self.access.next_asset_id(), Ordering::Release);
        self
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        self.range
            .end
            .fetch_min(self.access.next_asset_id(), Ordering::AcqRel);
    }
}
