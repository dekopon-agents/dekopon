use std::{
    fs::{File, FileTimes, TryLockError},
    io::Write as _,
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use serde::{Deserialize, Serialize};

use crate::{
    ContinuityPolicy, StorageGrantRequest, StorageHostError,
    key::{
        DOMAIN_AUDIT_SCOPE, DOMAIN_AUTHORITY, DOMAIN_GENERATION, DOMAIN_NAMESPACE_PATH, commitment,
        random_bytes, token,
    },
    layout::{
        Directory, ENTRY_CHARGE, EntryKind, EntryMetadata, Usage, scan_usage,
        usage_with_directory_entry,
    },
};

const POINTER_VERSION: &str = "dekopon.dev/storage-authority-pointer/v1alpha1";
const IDENTITY_VERSION: &str = "dekopon.dev/storage-resource/v1alpha1";

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ResourceIdentity {
    api_version: String,
    scope: dekopon_capability::StorageScope,
    values: Vec<String>,
}

fn identity(request: &StorageGrantRequest) -> ResourceIdentity {
    ResourceIdentity {
        api_version: IDENTITY_VERSION.to_owned(),
        scope: request.scope(),
        values: request.scope_values(),
    }
}

pub(crate) fn read_identity(
    base: &Directory,
) -> Result<Option<(dekopon_capability::StorageScope, Vec<String>)>, StorageHostError> {
    if !base.exists("identity")? {
        return Ok(None);
    }
    let document: ResourceIdentity = serde_json::from_slice(&base.read_bounded("identity", 4_096)?)
        .map_err(|error| {
            crate::report_decode_failure("storage-resource", &error);
            base.corrupt("identity", "storage-resource")
        })?;
    if document.api_version != IDENTITY_VERSION
        || document.values.len() > 7
        || document.values.iter().any(|value| value.len() > 1024)
    {
        return Err(base.corrupt("identity", "storage-resource"));
    }
    Ok(Some((document.scope, document.values)))
}

pub(crate) fn marker_time(base: &Directory) -> Result<Option<SystemTime>, StorageHostError> {
    if !base.exists("last-used")? {
        return Ok(None);
    }
    let file = base.open_private("last-used", false)?;
    let metadata = file.metadata().map_err(|source| base.io_error(source))?;
    if metadata.len() != 0 {
        return Err(base.corrupt("last-used", "last-used"));
    }
    metadata
        .modified()
        .map(Some)
        .map_err(|source| base.io_error(source))
}

fn touch_marker(base: &Directory, create: bool) -> Result<(), StorageHostError> {
    let file = if create {
        base.create_private("last-used")?
    } else {
        base.open_private("last-used", false)?
    };
    if file
        .metadata()
        .map_err(|source| base.io_error(source))?
        .len()
        != 0
    {
        return Err(base.corrupt("last-used", "last-used"));
    }
    let now = SystemTime::now();
    let previous = file
        .metadata()
        .and_then(|metadata| metadata.modified())
        .map_err(|source| base.io_error(source))?;
    file.set_times(FileTimes::new().set_modified(now.max(previous)))
        .map_err(|source| base.io_error(source))?;
    file.sync_all().map_err(|source| base.io_error(source))
}

#[derive(Debug)]
pub(crate) struct Namespace {
    pub(crate) base_token: String,
    pub(crate) generation_token: String,
    pub(crate) directory: Directory,
    pub(crate) data_directory: Directory,
    pub(crate) scope_commitment: String,
    pub(crate) _base_lease: File,
}

#[derive(Debug)]
pub(crate) struct Reset {
    pub(crate) check: &'static str,
    pub(crate) previous_generation: Option<String>,
    pub(crate) path: Option<PathBuf>,
}

pub(crate) struct NamespacePlan {
    base_token: String,
    scope_commitment: String,
    generation_token: String,
    authority_pointer: Option<Vec<u8>>,
    remove_authority_pointer: bool,
    set_aside: Option<String>,
    reset: Option<Reset>,
    existing_base: Option<Directory>,
    existing_base_lease: Option<File>,
    resource_identity: Vec<u8>,
    new_metadata: bool,
    before_usage: Usage,
    reserved_bytes: u64,
    reserved_entries: u64,
    maximum_generation_peak_bytes: u64,
}

struct Selection {
    generation_token: String,
    authority_pointer: Option<Vec<u8>>,
    removed_pointer_length: Option<u64>,
    retained_peak_bytes: Option<u64>,
    set_aside: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PointerDocument {
    api_version: String,
    authority: String,
    epoch: String,
}

impl NamespacePlan {
    pub(crate) fn prepare(
        namespaces_root: &Directory,
        request: &StorageGrantRequest,
        deadline: Instant,
        maximum_entries: u64,
    ) -> Result<Self, StorageHostError> {
        let values = request.scope_values();
        let fields = values.iter().map(String::as_bytes).collect::<Vec<_>>();
        let base_token = token(DOMAIN_NAMESPACE_PATH, &fields);
        let scope_commitment = commitment(DOMAIN_AUDIT_SCOPE, &fields);
        debug_assert_ne!(base_token, scope_commitment);
        let authority = token(DOMAIN_AUTHORITY, &[request.authority_surface()]);

        let (existing_base, existing_base_lease, before_usage, pointer_length) = (|| {
            if !namespaces_root.exists(&base_token)? {
                return Ok((None, None, Usage::default(), None));
            }
            let base = namespaces_root.open_directory(&base_token)?;
            let lease = base.open_private("base.lock", false)?;
            lock_exclusive(&lease, deadline)?;
            if !namespaces_root.retains_child(&base_token, &base)? {
                return Err(StorageHostError::Busy);
            }
            let usage = usage_with_directory_entry(scan_usage(&base, maximum_entries)?)?;
            let pointer_length = file_length(&base, "current")?;
            Ok((Some(base), Some(lease), usage, pointer_length))
        })()
        .map_err(|error: StorageHostError| error.in_namespace(&base_token, None))?;

        let expected = identity(request);
        let resource_identity = serde_json::to_vec(&expected).map_err(|error| {
            tracing::error!(error = %error, "could not encode storage identity");
            StorageHostError::Arithmetic
        })?;
        if resource_identity.len() > 4_096 {
            return Err(StorageHostError::QuotaExceeded);
        }
        let base = existing_base.as_ref();
        let new_metadata = if let Some(base) = base {
            match (read_identity(base)?, marker_time(base)?) {
                (Some((scope, values)), Some(_))
                    if scope == expected.scope && values == expected.values =>
                {
                    false
                }
                (None, None)
                    if request.scope() == dekopon_capability::StorageScope::PrivateConversation =>
                {
                    true
                }
                (Some(_), Some(_)) | (None, Some(_)) | (None, None) => {
                    return Err(base.corrupt("identity", "storage-resource-identity"));
                }
                (Some(_), None) => {
                    return Err(base.corrupt("last-used", "storage-resource-metadata"));
                }
            }
        } else {
            true
        };
        let select = |reset| {
            select_generation(
                base,
                &base_token,
                &authority,
                request.continuity_policy(),
                pointer_length,
                maximum_entries,
                reset,
            )
        };
        let (selection, reset) = match select(false) {
            Ok(selection) => (selection, None),
            Err(StorageHostError::Corrupt { scope, site }) => {
                let site = site.map(|site| *site).unwrap_or_default();
                (
                    select(true)?,
                    Some(Reset {
                        check: scope,
                        previous_generation: site.generation,
                        path: site.path,
                    }),
                )
            }
            Err(error) => return Err(error),
        };

        let mut simulation = Simulation::default();
        if base.is_none() {
            simulation.create_entry(0)?;
            simulation.create_entry(0)?;
        }
        let maximum_generation_peak_bytes = match selection.retained_peak_bytes {
            Some(peak) => peak,
            None => {
                simulation.create_entry(0)?;
                simulation.create_entry(0)?;
                simulation.create_entry(0)?;
                3_u64
                    .checked_mul(ENTRY_CHARGE)
                    .ok_or(StorageHostError::Arithmetic)?
            }
        };
        if new_metadata {
            simulation.create_entry(resource_identity.len() as u64)?;
            simulation.create_entry(0)?;
        }
        if let Some(pointer) = &selection.authority_pointer {
            simulation.replace(pointer_length, pointer.len() as u64)?;
        } else if let Some(length) = selection.removed_pointer_length {
            simulation.remove(length)?;
        }
        Ok(Self {
            base_token,
            scope_commitment,
            generation_token: selection.generation_token,
            authority_pointer: selection.authority_pointer,
            remove_authority_pointer: selection.removed_pointer_length.is_some(),
            set_aside: selection.set_aside,
            reset,
            existing_base,
            existing_base_lease,
            resource_identity,
            new_metadata,
            before_usage,
            reserved_bytes: simulation.peak_bytes,
            reserved_entries: simulation.peak_entries,
            maximum_generation_peak_bytes,
        })
    }

    pub(crate) const fn before_usage(&self) -> Usage {
        self.before_usage
    }

    pub(crate) const fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }

    pub(crate) const fn reserved_entries(&self) -> u64 {
        self.reserved_entries
    }

    pub(crate) const fn maximum_generation_peak_bytes(&self) -> u64 {
        self.maximum_generation_peak_bytes
    }

    pub(crate) const fn take_reset(&mut self) -> Option<Reset> {
        self.reset.take()
    }

    pub(crate) fn apply(
        mut self,
        namespaces_root: &Directory,
        deadline: Instant,
    ) -> Result<Namespace, StorageHostError> {
        let (base, base_lease) = match (self.existing_base.take(), self.existing_base_lease.take())
        {
            (Some(base), Some(lease)) => (base, lease),
            (None, None) => {
                let base = namespaces_root.ensure_directory(&self.base_token)?;
                let lease = base.open_private("base.lock", true)?;
                lock_exclusive(&lease, deadline)?;
                (base, lease)
            }
            _ => return Err(StorageHostError::corrupt("namespace-plan")),
        };

        if let Some(aside) = &self.set_aside {
            base.rename_to(&self.generation_token, &base, aside)?;
            base.sync()?;
        }
        let (directory, data_directory) = ensure_generation(&base, &self.generation_token)?;
        if let Some(pointer) = &self.authority_pointer {
            base.replace_private("current", pointer)?;
        } else if self.remove_authority_pointer {
            base.remove_file("current")?;
            base.sync()?;
        }
        if self.new_metadata {
            let mut file = base.create_private("identity")?;
            file.write_all(&self.resource_identity)
                .and_then(|()| file.sync_all())
                .map_err(|source| base.io_error(source))?;
            base.sync()?;
        }
        touch_marker(&base, self.new_metadata)?;
        Ok(Namespace {
            base_token: self.base_token,
            generation_token: self.generation_token,
            directory,
            data_directory,
            scope_commitment: self.scope_commitment,
            _base_lease: base_lease,
        })
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "every argument is one already-derived input of the single selection both passes share"
)]
fn select_generation(
    base: Option<&Directory>,
    base_token: &str,
    authority: &str,
    policy: ContinuityPolicy,
    pointer_length: Option<u64>,
    maximum_entries: u64,
    reset: bool,
) -> Result<Selection, StorageHostError> {
    match policy {
        ContinuityPolicy::Stable => {
            let generation_token = token(DOMAIN_GENERATION, &[base_token.as_bytes(), b"stable"]);
            let set_aside = match base {
                Some(base) if reset && base.exists(&generation_token)? => {
                    Some(crate::key::hex(&random_bytes(32)?))
                }
                _ => None,
            };
            let retained_peak_bytes = match base {
                Some(base) if set_aside.is_none() => {
                    retained_generation(base, base_token, &generation_token, maximum_entries)?
                }
                _ => None,
            };
            // A stale authority pointer must not survive stable continuity, or authority-bound A to
            // stable to A would reopen A's old epoch instead of minting a new non-reusing
            // generation.
            Ok(Selection {
                generation_token,
                authority_pointer: None,
                removed_pointer_length: pointer_length,
                retained_peak_bytes,
                set_aside,
            })
        }
        ContinuityPolicy::AuthorityBound => {
            let previous = match base {
                Some(base) if !reset => read_pointer(base, base_token)?,
                _ => None,
            };
            if let Some(base) = base
                && let Some(pointer) = previous.filter(|pointer| pointer.authority == authority)
            {
                let generation_token = token(
                    DOMAIN_GENERATION,
                    &[base_token.as_bytes(), pointer.epoch.as_bytes()],
                );
                let retained =
                    retained_generation(base, base_token, &generation_token, maximum_entries)?
                        .ok_or_else(|| {
                            base.corrupt(&generation_token, "missing-current-generation")
                                .in_namespace(base_token, Some(&generation_token))
                        })?;
                return Ok(Selection {
                    generation_token,
                    authority_pointer: None,
                    removed_pointer_length: None,
                    retained_peak_bytes: Some(retained),
                    set_aside: None,
                });
            }
            let epoch = crate::key::hex(&random_bytes(32)?);
            let generation_token = token(
                DOMAIN_GENERATION,
                &[base_token.as_bytes(), epoch.as_bytes()],
            );
            let authority_pointer = encode_pointer(authority.to_owned(), epoch)?;
            let retained_peak_bytes = match base {
                Some(base) => {
                    retained_generation(base, base_token, &generation_token, maximum_entries)?
                }
                None => None,
            };
            Ok(Selection {
                generation_token,
                authority_pointer: Some(authority_pointer),
                removed_pointer_length: None,
                retained_peak_bytes,
                set_aside: None,
            })
        }
    }
}

fn retained_generation(
    base: &Directory,
    base_token: &str,
    generation_token: &str,
    maximum_entries: u64,
) -> Result<Option<u64>, StorageHostError> {
    let located = |error: StorageHostError| error.in_namespace(base_token, Some(generation_token));
    let generation = match base.metadata(generation_token).map_err(located)? {
        None => return Ok(None),
        Some(metadata) if metadata.kind == EntryKind::Directory => {
            base.open_directory(generation_token).map_err(located)?
        }
        Some(_) => return Err(located(base.corrupt(generation_token, "generation-type"))),
    };
    (|| {
        let data = match generation.metadata("data")? {
            Some(metadata) if metadata.kind == EntryKind::Directory => {
                generation.open_directory("data")?
            }
            _ => return Err(generation.corrupt("data", "generation-layout")),
        };
        if !generation.exists("lease.lock")? {
            return Err(generation.corrupt("lease.lock", "generation-layout"));
        }
        let _ = generation.open_private("lease.lock", false)?;
        for token in data.entries_bounded(maximum_entries)? {
            logical_file(&data, &token)?;
        }
        Ok(Some(
            usage_with_directory_entry(scan_usage(&generation, maximum_entries)?)?.bytes,
        ))
    })()
    .map_err(located)
}

pub(crate) fn logical_file(
    data: &Directory,
    token: &str,
) -> Result<EntryMetadata, StorageHostError> {
    if !is_token(token) {
        return Err(data.corrupt(token, "logical-token"));
    }
    let metadata = data
        .metadata(token)?
        .ok_or_else(|| data.corrupt(token, "logical-token"))?;
    if metadata.kind != EntryKind::File || metadata.nlink != 1 {
        return Err(data.corrupt(token, "logical-file"));
    }
    let _ = data.open_private(token, false)?;
    Ok(metadata)
}

#[derive(Default)]
struct Simulation {
    current_bytes: i128,
    current_entries: i128,
    peak_bytes: u64,
    peak_entries: u64,
}

impl Simulation {
    fn create_entry(&mut self, length: u64) -> Result<(), StorageHostError> {
        self.current_entries = self
            .current_entries
            .checked_add(1)
            .ok_or(StorageHostError::Arithmetic)?;
        self.current_bytes = self
            .current_bytes
            .checked_add(i128::from(ENTRY_CHARGE))
            .and_then(|value| value.checked_add(i128::from(length)))
            .ok_or(StorageHostError::Arithmetic)?;
        self.observe()
    }

    fn replace(
        &mut self,
        old_length: Option<u64>,
        new_length: u64,
    ) -> Result<(), StorageHostError> {
        self.create_entry(new_length)?;
        if let Some(old_length) = old_length {
            self.current_entries = self
                .current_entries
                .checked_sub(1)
                .ok_or(StorageHostError::Arithmetic)?;
            self.current_bytes = self
                .current_bytes
                .checked_sub(i128::from(ENTRY_CHARGE) + i128::from(old_length))
                .ok_or(StorageHostError::Arithmetic)?;
        }
        self.observe()
    }

    fn remove(&mut self, old_length: u64) -> Result<(), StorageHostError> {
        self.current_entries = self
            .current_entries
            .checked_sub(1)
            .ok_or(StorageHostError::Arithmetic)?;
        self.current_bytes = self
            .current_bytes
            .checked_sub(i128::from(ENTRY_CHARGE) + i128::from(old_length))
            .ok_or(StorageHostError::Arithmetic)?;
        self.observe()
    }

    #[allow(
        clippy::map_err_ignore,
        reason = "both discarded values are TryFromIntError over a positive i128 accumulator, \
                  carrying only out-of-range, which Arithmetic already states"
    )]
    fn observe(&mut self) -> Result<(), StorageHostError> {
        if self.current_bytes > 0 {
            self.peak_bytes = self
                .peak_bytes
                .max(u64::try_from(self.current_bytes).map_err(|_| StorageHostError::Arithmetic)?);
        }
        if self.current_entries > 0 {
            self.peak_entries = self.peak_entries.max(
                u64::try_from(self.current_entries).map_err(|_| StorageHostError::Arithmetic)?,
            );
        }
        Ok(())
    }
}

fn file_length(parent: &Directory, name: &str) -> Result<Option<u64>, StorageHostError> {
    match parent.metadata(name)? {
        None => Ok(None),
        Some(metadata) if metadata.kind == EntryKind::File && metadata.nlink == 1 => {
            let _ = parent.open_private(name, false)?;
            Ok(Some(metadata.len))
        }
        Some(_) => Err(parent.corrupt(name, "housekeeping-file")),
    }
}

#[allow(
    clippy::map_err_ignore,
    reason = "serializing this owned all-string pointer struct has no failing case; serde_json \
              fails only on a non-string map key or a Serialize implementation error, and neither \
              exists here"
)]
fn encode_pointer(authority: String, epoch: String) -> Result<Vec<u8>, StorageHostError> {
    serde_json::to_vec(&PointerDocument {
        api_version: POINTER_VERSION.to_owned(),
        authority,
        epoch,
    })
    .map_err(|_| StorageHostError::corrupt("authority-pointer"))
}

fn ensure_generation(
    base_directory: &Directory,
    generation: &str,
) -> Result<(Directory, Directory), StorageHostError> {
    let directory = base_directory.ensure_directory(generation)?;
    let data = directory.ensure_directory("data")?;
    let lease = directory.open_private("lease.lock", true)?;
    lease
        .sync_all()
        .map_err(|source| directory.io_error(source))?;
    directory.sync()?;
    Ok((directory, data))
}

fn read_pointer(
    base_directory: &Directory,
    base_token: &str,
) -> Result<Option<PointerDocument>, StorageHostError> {
    if !base_directory.exists("current")? {
        return Ok(None);
    }
    let document: PointerDocument = serde_json::from_slice(
        &base_directory.read_bounded("current", 4_096)?,
    )
    .map_err(|error| {
        crate::report_decode_failure("authority-pointer", &error);
        base_directory.corrupt("current", "authority-pointer")
    })?;
    if document.api_version != POINTER_VERSION
        || !is_token(&document.epoch)
        || !is_token(&document.authority)
    {
        let previous = is_token(&document.epoch).then(|| {
            token(
                DOMAIN_GENERATION,
                &[base_token.as_bytes(), document.epoch.as_bytes()],
            )
        });
        return Err(base_directory
            .corrupt("current", "authority-pointer")
            .in_namespace(base_token, previous.as_deref()));
    }
    Ok(Some(document))
}

pub(crate) fn deadline_after(timeout_ms: u64) -> Result<Instant, StorageHostError> {
    Instant::now()
        .checked_add(Duration::from_millis(timeout_ms))
        .ok_or(StorageHostError::Arithmetic)
}

/// Timeout means another holder still has the lease and is worth retrying; Io means the filesystem
/// itself refuses advisory locks and retrying won't help.
pub(crate) fn lock_exclusive(file: &File, deadline: Instant) -> Result<(), StorageHostError> {
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(error) => match lease_lock_failure(error, Instant::now() >= deadline) {
                Some(failure) => return Err(failure),
                None => std::thread::sleep(Duration::from_millis(5)),
            },
        }
    }
}

/// `flock` refuses a pipe on some platforms and accepts it on others, so this is tested directly
/// rather than reported as `Timeout`, which would suggest endless retrying.
fn lease_lock_failure(error: TryLockError, expired: bool) -> Option<StorageHostError> {
    match error {
        TryLockError::WouldBlock if !expired => None,
        TryLockError::WouldBlock => Some(StorageHostError::Timeout),
        TryLockError::Error(_) => Some(StorageHostError::Io),
    }
}

pub(crate) fn is_token(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{File, TryLockError},
        io::{Error, ErrorKind},
        time::{Duration, Instant},
    };

    use super::{deadline_after, lease_lock_failure, lock_exclusive};
    use crate::StorageHostError;

    #[test]
    fn a_lease_another_holder_still_has_reports_timeout_rather_than_io() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("base.lock");
        let held = File::create(&path).expect("lease file");
        held.try_lock().expect("the first holder takes the lease");

        let contender = File::open(&path).expect("a second handle on the same lease");
        let started = Instant::now();
        let refused = lock_exclusive(&contender, deadline_after(30).expect("deadline"));

        assert!(
            matches!(refused, Err(StorageHostError::Timeout)),
            "{refused:?}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(30),
            "the deadline was reported without being waited out"
        );

        held.unlock().expect("the first holder releases the lease");
        lock_exclusive(&contender, deadline_after(30).expect("deadline"))
            .expect("an uncontended lease is taken");
    }

    #[test]
    fn a_lock_failure_that_is_not_contention_is_io_at_either_side_of_the_deadline() {
        assert!(lease_lock_failure(TryLockError::WouldBlock, false).is_none());
        assert!(matches!(
            lease_lock_failure(TryLockError::WouldBlock, true),
            Some(StorageHostError::Timeout)
        ));

        for expired in [false, true] {
            let failure = lease_lock_failure(
                TryLockError::Error(Error::from(ErrorKind::Unsupported)),
                expired,
            );
            assert!(matches!(failure, Some(StorageHostError::Io)), "{failure:?}");
        }
    }
}
