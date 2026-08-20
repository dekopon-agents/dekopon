//! Non-reusing namespace derivation, exact housekeeping planning, and continuity pointers.

use std::{
    fs::File,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::{
    ContinuityPolicy, StorageGrantRequest, StorageHostError,
    key::{
        DOMAIN_AUDIT_SCOPE, DOMAIN_AUTHORITY, DOMAIN_GENERATION, DOMAIN_NAMESPACE_PATH, StorageKey,
        random_bytes,
    },
    layout::{Directory, ENTRY_CHARGE, EntryKind, Usage, scan_usage},
};

const POINTER_VERSION: &str = "dekopon.dev/storage-authority-pointer/v1alpha1";

#[derive(Debug)]
pub(crate) struct Namespace {
    pub(crate) base_token: String,
    pub(crate) generation_token: String,
    pub(crate) base_directory: Directory,
    pub(crate) directory: Directory,
    pub(crate) data_directory: Directory,
    pub(crate) scope_commitment: String,
    pub(crate) record_key: [u8; 32],
    /// Held before the generation lease is acquired and retained through transaction finalization.
    pub(crate) _base_lease: File,
}

/// A fully materialized, non-mutating namespace plan.
///
/// Random epochs, timestamps, MACed documents, existing target lengths, and the base lease are all
/// fixed here. The host can therefore reserve the exact peak before [`apply`](Self::apply) performs
/// the first mutation.
pub(crate) struct NamespacePlan {
    base_token: String,
    scope_commitment: String,
    generation_token: String,
    authority_pointer: Option<Vec<u8>>,
    remove_authority_pointer: bool,
    previous_generation: Option<String>,
    retired_marker: Option<Vec<u8>>,
    last_access_marker: Vec<u8>,
    existing_base: Option<Directory>,
    existing_base_lease: Option<File>,
    before_usage: Usage,
    reserved_bytes: u64,
    reserved_entries: u64,
    maximum_generation_peak_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PointerBody {
    api_version: String,
    authority: String,
    epoch: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PointerDocument {
    api_version: String,
    authority: String,
    epoch: String,
    mac: String,
}

impl NamespacePlan {
    pub(crate) fn prepare(
        namespaces_root: &Directory,
        key: &StorageKey,
        request: &StorageGrantRequest,
        lock_timeout_ms: u64,
        maximum_entries: u64,
    ) -> Result<Self, StorageHostError> {
        let values = request.scope_values();
        let fields = values.iter().map(String::as_bytes).collect::<Vec<_>>();
        let base_token = key.token(DOMAIN_NAMESPACE_PATH, &fields);
        let scope_commitment = key.commitment(DOMAIN_AUDIT_SCOPE, &fields);
        debug_assert_ne!(base_token, scope_commitment);
        let authority = key.token(DOMAIN_AUTHORITY, &[request.authority_surface()]);

        let (existing_base, existing_base_lease, before_usage) =
            if namespaces_root.exists(&base_token)? {
                let base = namespaces_root.open_directory(&base_token)?;
                let lease = base.open_private("base.lock", false)?;
                lock_exclusive(&lease, lock_timeout_ms)?;
                if !namespaces_root.retains_child(&base_token, &base)? {
                    return Err(StorageHostError::Busy);
                }
                if base.exists("poisoned")? {
                    if file_length(&base, "poisoned")? != Some(0) {
                        return Err(StorageHostError::Corrupt {
                            scope: "poison-marker",
                        });
                    }
                    return Err(StorageHostError::Corrupt {
                        scope: "poisoned-namespace",
                    });
                }
                let mut usage = scan_usage(&base, maximum_entries)?;
                usage.entries = usage
                    .entries
                    .checked_add(1)
                    .ok_or(StorageHostError::Arithmetic)?;
                usage.bytes = usage
                    .bytes
                    .checked_add(ENTRY_CHARGE)
                    .ok_or(StorageHostError::Arithmetic)?;
                (Some(base), Some(lease), usage)
            } else {
                (None, None, Usage::default())
            };

        let previous_pointer = match &existing_base {
            Some(base) => read_pointer(base, key, &base_token)?,
            None => None,
        };
        let stable_generation = key.token(DOMAIN_GENERATION, &[base_token.as_bytes(), b"stable"]);
        let stable_exists = match &existing_base {
            Some(base) => match base.metadata(&stable_generation)? {
                None => false,
                Some(metadata) if metadata.kind == EntryKind::Directory => true,
                Some(_) => {
                    return Err(StorageHostError::Corrupt {
                        scope: "stable-generation-type",
                    });
                }
            },
            None => false,
        };
        let (
            generation_token,
            authority_pointer,
            remove_authority_pointer,
            previous_generation,
            generation_must_exist,
        ) = match request.continuity_policy() {
            ContinuityPolicy::Stable => {
                // A stale authority pointer must not survive a period of explicit stable
                // continuity. Otherwise authority-bound A -> stable -> A would reopen A's old
                // random epoch instead of minting the required non-reusing generation.
                let previous = previous_pointer.as_ref().map(|pointer| {
                    key.token(
                        DOMAIN_GENERATION,
                        &[base_token.as_bytes(), pointer.epoch.as_bytes()],
                    )
                });
                (
                    stable_generation.clone(),
                    None,
                    previous_pointer.is_some(),
                    previous,
                    false,
                )
            }
            ContinuityPolicy::AuthorityBound => {
                if let Some(pointer) = &previous_pointer
                    && pointer.authority == authority
                {
                    (
                        key.token(
                            DOMAIN_GENERATION,
                            &[base_token.as_bytes(), pointer.epoch.as_bytes()],
                        ),
                        None,
                        false,
                        None,
                        true,
                    )
                } else {
                    let epoch = crate::key::hex(&random_bytes(32)?);
                    let generation = key.token(
                        DOMAIN_GENERATION,
                        &[base_token.as_bytes(), epoch.as_bytes()],
                    );
                    let pointer = encode_pointer(key, &base_token, authority, epoch)?;
                    let previous = previous_pointer
                        .map(|pointer| {
                            key.token(
                                DOMAIN_GENERATION,
                                &[base_token.as_bytes(), pointer.epoch.as_bytes()],
                            )
                        })
                        .or_else(|| stable_exists.then(|| stable_generation.clone()));
                    (generation, Some(pointer), false, previous, false)
                }
            }
        };

        let timestamp = now_ms()?.to_string();
        let last_access_marker = lifecycle_marker(
            key,
            b"last-access",
            &base_token,
            &generation_token,
            &timestamp,
        );
        let retired_marker = previous_generation.as_ref().map(|generation| {
            lifecycle_marker(key, b"retired", &base_token, generation, &timestamp)
        });

        let mut simulation = Simulation::default();
        let base = existing_base.as_ref();
        if base.is_none() {
            simulation.create_entry(0)?; // namespace base directory
            simulation.create_entry(0)?; // base.lock
        }
        let generation = base
            .and_then(|base| open_optional_directory(base, &generation_token).transpose())
            .transpose()?;
        if generation_must_exist && generation.is_none() {
            return Err(StorageHostError::Corrupt {
                scope: "missing-current-generation",
            });
        }
        let maximum_generation_peak_bytes;
        if let Some(generation) = &generation {
            require_directory(generation, "data")?;
            require_private_file(generation, "lease.lock")?;
            let usage = usage_with_directory_entry(scan_usage(generation, maximum_entries)?)?;
            maximum_generation_peak_bytes = replacement_peak(
                usage.bytes,
                file_length(generation, "last-access")?,
                last_access_marker.len() as u64,
            )?;
        } else {
            simulation.create_entry(0)?; // generation directory
            simulation.create_entry(0)?; // data directory
            simulation.create_entry(0)?; // lease.lock
            maximum_generation_peak_bytes = 4_u64
                .checked_mul(ENTRY_CHARGE)
                .and_then(|bytes| bytes.checked_add(last_access_marker.len() as u64))
                .ok_or(StorageHostError::Arithmetic)?;
        }

        let old_pointer_length = base
            .and_then(|base| file_length(base, "current").transpose())
            .transpose()?;
        if let Some(pointer) = &authority_pointer {
            simulation.replace(old_pointer_length, pointer.len() as u64)?;
        } else if remove_authority_pointer {
            simulation.remove(old_pointer_length.ok_or(StorageHostError::Corrupt {
                scope: "missing-authority-pointer",
            })?)?;
        }
        if let (Some(previous), Some(marker)) = (&previous_generation, &retired_marker)
            && let Some(base) = base
        {
            let previous = base.open_directory(previous)?;
            // A retired generation remains root-accounted under the limits that admitted it. A
            // later namespace-limit reduction must still be able to rotate into a new empty
            // generation rather than being blocked by historical bytes it is trying to leave.
            simulation.replace(file_length(&previous, "retired")?, marker.len() as u64)?;
        }
        simulation.replace(
            generation
                .as_ref()
                .map(|generation| file_length(generation, "last-access"))
                .transpose()?
                .flatten(),
            last_access_marker.len() as u64,
        )?;

        Ok(Self {
            base_token,
            scope_commitment,
            generation_token,
            authority_pointer,
            remove_authority_pointer,
            previous_generation,
            retired_marker,
            last_access_marker,
            existing_base,
            existing_base_lease,
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

    pub(crate) fn apply(
        mut self,
        namespaces_root: &Directory,
        key: &StorageKey,
        lock_timeout_ms: u64,
    ) -> Result<Namespace, StorageHostError> {
        let (base, base_lease) = match (self.existing_base.take(), self.existing_base_lease.take())
        {
            (Some(base), Some(lease)) => (base, lease),
            (None, None) => {
                let base = namespaces_root.ensure_directory(&self.base_token)?;
                let lease = base.open_private("base.lock", true)?;
                lock_exclusive(&lease, lock_timeout_ms)?;
                (base, lease)
            }
            _ => {
                return Err(StorageHostError::Corrupt {
                    scope: "namespace-plan",
                });
            }
        };

        let (directory, data_directory) = ensure_generation(&base, &self.generation_token)?;
        if let Some(pointer) = &self.authority_pointer {
            base.replace_private("current", pointer)?;
        } else if self.remove_authority_pointer {
            // Removing the authority pointer publishes stable mode. Retirement follows that
            // publication, so a failure cannot make the still-current authority generation
            // collectable while its pointer continues to name it.
            base.remove_file("current")?;
            base.sync()?;
        }
        // Retirement follows pointer publication/removal. A failure may delay collection of the
        // old generation, but can never make a still-current generation GC-eligible.
        if let (Some(previous), Some(marker)) = (&self.previous_generation, &self.retired_marker)
            && previous != &self.generation_token
        {
            let old = base.open_directory(previous)?;
            old.replace_private("retired", marker)?;
        }
        directory.replace_private("last-access", &self.last_access_marker)?;
        let record_key = key.bytes(
            crate::key::DOMAIN_RECORD_ID,
            &[self.base_token.as_bytes(), self.generation_token.as_bytes()],
        );
        Ok(Namespace {
            base_token: self.base_token,
            generation_token: self.generation_token,
            base_directory: base,
            directory,
            data_directory,
            scope_commitment: self.scope_commitment,
            record_key,
            _base_lease: base_lease,
        })
    }
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

    /// Simulates unique create-new temporary + sync + same-directory replacement.
    fn replace(
        &mut self,
        old_length: Option<u64>,
        new_length: u64,
    ) -> Result<(), StorageHostError> {
        // The temporary coexists with the old target.
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

    fn observe(&mut self) -> Result<(), StorageHostError> {
        // A plan may shrink an existing pointer/marker, so its net delta can be negative. Only the
        // positive peak needs reserving above the already-accounted baseline.
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

fn usage_with_directory_entry(mut usage: Usage) -> Result<Usage, StorageHostError> {
    usage.entries = usage
        .entries
        .checked_add(1)
        .ok_or(StorageHostError::Arithmetic)?;
    usage.bytes = usage
        .bytes
        .checked_add(ENTRY_CHARGE)
        .ok_or(StorageHostError::Arithmetic)?;
    Ok(usage)
}

fn replacement_peak(
    baseline: u64,
    old_length: Option<u64>,
    new_length: u64,
) -> Result<u64, StorageHostError> {
    let temporary = ENTRY_CHARGE
        .checked_add(new_length)
        .ok_or(StorageHostError::Arithmetic)?;
    let peak = baseline
        .checked_add(temporary)
        .ok_or(StorageHostError::Arithmetic)?;
    if let Some(old_length) = old_length {
        let _final = peak
            .checked_sub(
                ENTRY_CHARGE
                    .checked_add(old_length)
                    .ok_or(StorageHostError::Arithmetic)?,
            )
            .ok_or(StorageHostError::Arithmetic)?;
    }
    Ok(peak)
}

fn open_optional_directory(
    parent: &Directory,
    name: &str,
) -> Result<Option<Directory>, StorageHostError> {
    match parent.metadata(name)? {
        None => Ok(None),
        Some(metadata) if metadata.kind == EntryKind::Directory => {
            parent.open_directory(name).map(Some)
        }
        Some(_) => Err(StorageHostError::Corrupt {
            scope: "generation-type",
        }),
    }
}

fn require_directory(parent: &Directory, name: &str) -> Result<(), StorageHostError> {
    if parent
        .metadata(name)?
        .is_some_and(|metadata| metadata.kind == EntryKind::Directory)
    {
        let _ = parent.open_directory(name)?;
        Ok(())
    } else {
        Err(StorageHostError::Corrupt {
            scope: "generation-layout",
        })
    }
}

fn require_private_file(parent: &Directory, name: &str) -> Result<(), StorageHostError> {
    let _ = parent.open_private(name, false)?;
    Ok(())
}

fn file_length(parent: &Directory, name: &str) -> Result<Option<u64>, StorageHostError> {
    match parent.metadata(name)? {
        None => Ok(None),
        Some(metadata) if metadata.kind == EntryKind::File && metadata.nlink == 1 => {
            let _ = parent.open_private(name, false)?;
            Ok(Some(metadata.len))
        }
        Some(_) => Err(StorageHostError::Corrupt {
            scope: "housekeeping-file",
        }),
    }
}

fn encode_pointer(
    key: &StorageKey,
    base_token: &str,
    authority: String,
    epoch: String,
) -> Result<Vec<u8>, StorageHostError> {
    let body = PointerBody {
        api_version: POINTER_VERSION.to_owned(),
        authority,
        epoch,
    };
    let encoded = serde_json::to_vec(&body).map_err(|_| StorageHostError::Corrupt {
        scope: "authority-pointer",
    })?;
    let document = PointerDocument {
        api_version: body.api_version,
        authority: body.authority,
        epoch: body.epoch,
        mac: key.commitment(
            crate::key::DOMAIN_MANIFEST,
            &[base_token.as_bytes(), encoded.as_slice()],
        ),
    };
    serde_json::to_vec(&document).map_err(|_| StorageHostError::Corrupt {
        scope: "authority-pointer",
    })
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
    key: &StorageKey,
    base_token: &str,
) -> Result<Option<PointerDocument>, StorageHostError> {
    if !base_directory.exists("current")? {
        return Ok(None);
    }
    let document: PointerDocument =
        serde_json::from_slice(&base_directory.read_bounded("current", 4_096)?).map_err(|_| {
            StorageHostError::Corrupt {
                scope: "authority-pointer",
            }
        })?;
    let body = PointerBody {
        api_version: document.api_version.clone(),
        authority: document.authority.clone(),
        epoch: document.epoch.clone(),
    };
    let encoded = serde_json::to_vec(&body).map_err(|_| StorageHostError::Corrupt {
        scope: "authority-pointer",
    })?;
    let expected = key.commitment(
        crate::key::DOMAIN_MANIFEST,
        &[base_token.as_bytes(), encoded.as_slice()],
    );
    if document.api_version != POINTER_VERSION
        || document.mac != expected
        || !is_token(&document.epoch)
        || !is_token(&document.authority)
    {
        return Err(StorageHostError::Corrupt {
            scope: "authority-pointer",
        });
    }
    Ok(Some(document))
}

pub(crate) fn current_generation(
    base_directory: &Directory,
    key: &StorageKey,
    base_token: &str,
) -> Result<Option<String>, StorageHostError> {
    Ok(
        read_pointer(base_directory, key, base_token)?.map(|document| {
            key.token(
                DOMAIN_GENERATION,
                &[base_token.as_bytes(), document.epoch.as_bytes()],
            )
        }),
    )
}

/// Validates one complete isolated namespace without following or trusting any path entry.
///
/// Root/layout corruption remains fatal, while callers may quarantine an error returned here and
/// continue serving independently healthy namespace bases.
pub(crate) fn validate_namespace_base(
    base_directory: &Directory,
    key: &StorageKey,
    base_token: &str,
) -> Result<(), StorageHostError> {
    if !is_token(base_token) {
        return Err(StorageHostError::Corrupt {
            scope: "namespace-token",
        });
    }
    let mut generations = Vec::new();
    let mut saw_base_lease = false;
    for name in base_directory.entries()? {
        let metadata = base_directory
            .metadata(&name)?
            .ok_or(StorageHostError::Corrupt {
                scope: "namespace-entry",
            })?;
        match (name.as_str(), metadata.kind) {
            ("base.lock", EntryKind::File) => {
                if file_length(base_directory, "base.lock")? != Some(0) {
                    return Err(StorageHostError::Corrupt {
                        scope: "base-lease",
                    });
                }
                saw_base_lease = true;
            }
            ("current", EntryKind::File) => {
                // Full decoding/MAC verification follows below.
                let _ = file_length(base_directory, "current")?;
            }
            ("poisoned", EntryKind::File) => {
                if file_length(base_directory, "poisoned")? != Some(0) {
                    return Err(StorageHostError::Corrupt {
                        scope: "poison-marker",
                    });
                }
            }
            (_, EntryKind::Directory) if is_token(&name) => generations.push(name),
            _ => {
                return Err(StorageHostError::Corrupt {
                    scope: "namespace-entry",
                });
            }
        }
    }
    if !saw_base_lease || generations.is_empty() {
        return Err(StorageHostError::Corrupt {
            scope: "namespace-layout",
        });
    }

    let current = current_generation(base_directory, key, base_token)?;
    if current
        .as_ref()
        .is_some_and(|current| !generations.contains(current))
    {
        return Err(StorageHostError::Corrupt {
            scope: "missing-current-generation",
        });
    }
    let stable = key.token(DOMAIN_GENERATION, &[base_token.as_bytes(), b"stable"]);
    if current.is_none() && !generations.contains(&stable) {
        return Err(StorageHostError::Corrupt {
            scope: "missing-stable-generation",
        });
    }

    for generation_token in generations {
        let generation = base_directory.open_directory(&generation_token)?;
        validate_generation(&generation, key, base_token, &generation_token)?;
        if current.as_deref() == Some(generation_token.as_str())
            && lifecycle_timestamp(&generation, "retired", key, base_token, &generation_token)?
                .is_some()
        {
            return Err(StorageHostError::Corrupt {
                scope: "retired-current-generation",
            });
        }
    }
    Ok(())
}

fn validate_generation(
    generation: &Directory,
    key: &StorageKey,
    base_token: &str,
    generation_token: &str,
) -> Result<(), StorageHostError> {
    let mut saw_data = false;
    let mut saw_lease = false;
    let mut saw_last_access = false;
    for name in generation.entries()? {
        let metadata = generation
            .metadata(&name)?
            .ok_or(StorageHostError::Corrupt {
                scope: "generation-entry",
            })?;
        match (name.as_str(), metadata.kind) {
            ("data", EntryKind::Directory) => {
                let data = generation.open_directory("data")?;
                for token in data.entries()? {
                    if !is_token(&token) {
                        return Err(StorageHostError::Corrupt {
                            scope: "logical-token",
                        });
                    }
                    let metadata = data.metadata(&token)?.ok_or(StorageHostError::Corrupt {
                        scope: "logical-file",
                    })?;
                    if metadata.kind != EntryKind::File || metadata.nlink != 1 {
                        return Err(StorageHostError::Corrupt {
                            scope: "logical-file",
                        });
                    }
                    let _ = data.open_private(&token, false)?;
                }
                saw_data = true;
            }
            ("lease.lock", EntryKind::File) => {
                if file_length(generation, "lease.lock")? != Some(0) {
                    return Err(StorageHostError::Corrupt {
                        scope: "generation-lease",
                    });
                }
                saw_lease = true;
            }
            ("last-access", EntryKind::File) => {
                lifecycle_timestamp(generation, "last-access", key, base_token, generation_token)?
                    .ok_or(StorageHostError::Corrupt {
                        scope: "last-access-marker",
                    })?;
                saw_last_access = true;
            }
            ("retired", EntryKind::File) => {
                lifecycle_timestamp(generation, "retired", key, base_token, generation_token)?
                    .ok_or(StorageHostError::Corrupt {
                        scope: "retired-marker",
                    })?;
            }
            ("poisoned", EntryKind::File) => {
                if file_length(generation, "poisoned")? != Some(0) {
                    return Err(StorageHostError::Corrupt {
                        scope: "poison-marker",
                    });
                }
            }
            _ => {
                return Err(StorageHostError::Corrupt {
                    scope: "generation-entry",
                });
            }
        }
    }
    if !saw_data || !saw_lease || !saw_last_access {
        return Err(StorageHostError::Corrupt {
            scope: "generation-layout",
        });
    }
    Ok(())
}

fn lifecycle_marker(
    key: &StorageKey,
    label: &[u8],
    base: &str,
    generation: &str,
    timestamp: &str,
) -> Vec<u8> {
    let commitment = key.commitment(
        crate::key::DOMAIN_LIFECYCLE,
        &[
            label,
            base.as_bytes(),
            generation.as_bytes(),
            timestamp.as_bytes(),
        ],
    );
    format!("{timestamp}\n{commitment}\n").into_bytes()
}

pub(crate) fn lifecycle_timestamp(
    directory: &Directory,
    marker: &str,
    key: &StorageKey,
    base: &str,
    generation: &str,
) -> Result<Option<u64>, StorageHostError> {
    if !directory.exists(marker)? {
        return Ok(None);
    }
    let bytes = directory.read_bounded(marker, 4_096)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| StorageHostError::Corrupt {
        scope: "lifecycle-marker",
    })?;
    let mut lines = text.lines();
    let timestamp_text = lines.next().ok_or(StorageHostError::Corrupt {
        scope: "lifecycle-marker",
    })?;
    let commitment = lines.next().ok_or(StorageHostError::Corrupt {
        scope: "lifecycle-marker",
    })?;
    if lines.next().is_some() {
        return Err(StorageHostError::Corrupt {
            scope: "lifecycle-marker",
        });
    }
    let timestamp = timestamp_text
        .parse::<u64>()
        .map_err(|_| StorageHostError::Corrupt {
            scope: "lifecycle-marker",
        })?;
    let label = match marker {
        "retired" => b"retired".as_slice(),
        "last-access" => b"last-access".as_slice(),
        _ => {
            return Err(StorageHostError::Corrupt {
                scope: "lifecycle-marker",
            });
        }
    };
    let expected = key.commitment(
        crate::key::DOMAIN_LIFECYCLE,
        &[
            label,
            base.as_bytes(),
            generation.as_bytes(),
            timestamp_text.as_bytes(),
        ],
    );
    if commitment != expected {
        return Err(StorageHostError::Corrupt {
            scope: "lifecycle-marker-mac",
        });
    }
    Ok(Some(timestamp))
}

pub(crate) fn lock_exclusive(file: &File, timeout_ms: u64) -> Result<(), StorageHostError> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(timeout_ms))
        .ok_or(StorageHostError::Arithmetic)?;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(StorageHostError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Err(StorageHostError::Io),
        }
    }
}

fn now_ms() -> Result<u64, StorageHostError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageHostError::Clock)?;
    u64::try_from(duration.as_millis()).map_err(|_| StorageHostError::Arithmetic)
}

pub(crate) fn is_token(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
