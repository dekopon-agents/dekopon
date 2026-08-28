//! Bounded retired-generation and inactive-namespace garbage collection.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    StorageHostError, StorageLimits,
    key::StorageKey,
    layout::{Directory, ENTRY_CHARGE, EntryKind, EntryStream, Layout, Usage, scan_usage_capped},
    namespace::{current_generation, is_token, lifecycle_timestamp},
    quota::QuotaLedger,
};

/// Content-free result of one lifecycle pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    pub namespaces_removed: u64,
    pub bytes_removed: u64,
}

/// Retained directory streams rotate bounded passes past an ineligible first entry.
///
/// The cursors are process-persistent rather than filesystem metadata: restart recovery validates
/// every retained state before GC starts, and repeated passes cannot starve later trash or bases.
#[derive(Debug, Default)]
pub(crate) struct GcState {
    trash: Option<EntryStream>,
    namespaces: Option<EntryStream>,
    generations: BTreeMap<String, EntryStream>,
}

pub(crate) fn run(
    layout: &Layout,
    key: &StorageKey,
    limits: &StorageLimits,
    namespace_locks: &Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    namespace_observation_lock: &Mutex<()>,
    ledger: &Arc<QuotaLedger>,
    state: &mut GcState,
) -> Result<GcReport, StorageHostError> {
    #[allow(
        clippy::map_err_ignore,
        reason = "SystemTimeError carries only how far the clock sits before the epoch and \
                  TryFromIntError only out-of-range; Clock and Arithmetic already state both, and \
                  neither value may be exported as storage telemetry"
    )]
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StorageHostError::Clock)?
            .as_millis(),
    )
    .map_err(|_| StorageHostError::Arithmetic)?;
    let mut report = GcReport::default();

    // Trash is already unreachable and quota-accounted. Inspect at most the configured number of
    // entries, and retain the stream position even when an unknown transaction cannot be removed.
    for _ in 0..limits.gc_max_namespaces_per_pass {
        if mutation_or_byte_limit(&report, limits) {
            return Ok(report);
        }
        let Some(name) = next_rotating(layout.trash(), &mut state.trash)? else {
            break;
        };
        let metadata = layout
            .trash()
            .metadata(&name)?
            .ok_or(StorageHostError::Corrupt {
                scope: "trash-entry",
            })?;
        if metadata.kind == EntryKind::Directory
            && name.starts_with("transaction-")
            && committed_transaction_is_unknown(&layout.trash().open_directory(&name)?)?
        {
            continue;
        }
        let remaining = limits
            .gc_max_bytes_per_pass
            .saturating_sub(report.bytes_removed);
        let Some(usage) = entry_usage_capped(layout.trash(), &name, limits, remaining)? else {
            continue;
        };
        layout.trash().remove_tree(&name)?;
        // Reconcile immediately after successful unlink. A following parent-sync failure is a
        // crash-durability uncertainty, not a reason to leave an absent entry charged forever in
        // this process; restart reconstructs whichever state actually survived.
        reconcile_removed_trash(ledger, &name);
        ledger.release_root_usage(usage);
        report.namespaces_removed = report.namespaces_removed.saturating_add(1);
        report.bytes_removed = report.bytes_removed.saturating_add(usage.bytes);
        layout.trash().sync()?;
    }

    let mut inspected_bases = 0_u64;
    let mut inspected_generation_entries = 0_u64;
    while inspected_bases < limits.gc_max_namespaces_per_pass
        && !mutation_or_byte_limit(&report, limits)
    {
        let Some(base_token) = next_rotating(layout.namespaces(), &mut state.namespaces)? else {
            break;
        };
        inspected_bases = inspected_bases.saturating_add(1);
        if !is_token(&base_token) {
            return Err(StorageHostError::Corrupt {
                scope: "namespace-token",
            });
        }

        // Grants take the same per-base housekeeping lock before opening or waiting on base.lock.
        let namespace_lock = crate::namespace_lock(namespace_locks, &base_token);
        let _housekeeping = namespace_lock
            .lock()
            .expect("storage namespace housekeeping lock");
        let metadata =
            layout
                .namespaces()
                .metadata(&base_token)?
                .ok_or(StorageHostError::Corrupt {
                    scope: "namespace-type",
                })?;
        if metadata.kind != EntryKind::Directory {
            return Err(StorageHostError::Corrupt {
                scope: "namespace-type",
            });
        }
        let base = layout.namespaces().open_directory(&base_token)?;
        let base_lease = base.open_private("base.lock", false)?;
        if base_lease.try_lock().is_err() {
            continue;
        }
        if base.exists("poisoned")? {
            continue;
        }
        let current = current_generation(&base, key, &base_token)?;

        let mut generation_stream = state
            .generations
            .remove(&base_token)
            .map(Ok)
            .unwrap_or_else(|| base.entry_stream())?;
        let mut generation_stream_exhausted = false;
        while inspected_generation_entries < limits.gc_max_namespaces_per_pass
            && !mutation_or_byte_limit(&report, limits)
        {
            let Some(name) = generation_stream.next_name()? else {
                generation_stream_exhausted = true;
                break;
            };
            inspected_generation_entries = inspected_generation_entries.saturating_add(1);
            let metadata = base.metadata(&name)?.ok_or(StorageHostError::Corrupt {
                scope: "namespace-entry",
            })?;
            match metadata.kind {
                EntryKind::File
                    if matches!(name.as_str(), "base.lock" | "current" | "poisoned") =>
                {
                    continue;
                }
                EntryKind::Directory if is_token(&name) => {}
                EntryKind::Directory => {
                    return Err(StorageHostError::Corrupt {
                        scope: "generation-token",
                    });
                }
                _ => {
                    return Err(StorageHostError::Corrupt {
                        scope: "namespace-entry",
                    });
                }
            }
            if current.as_deref() == Some(name.as_str()) {
                continue;
            }
            let generation = base.open_directory(&name)?;
            let (timestamp, required_age) = if let Some(timestamp) =
                lifecycle_timestamp(&generation, "retired", key, &base_token, &name)?
            {
                (
                    timestamp,
                    limits
                        .retired_generation_grace_ms
                        .max(limits.retired_generation_ttl_ms),
                )
            } else if let Some(timestamp) =
                lifecycle_timestamp(&generation, "last-access", key, &base_token, &name)?
            {
                (timestamp, limits.inactive_namespace_ttl_ms)
            } else {
                continue;
            };
            if now.saturating_sub(timestamp) < required_age {
                continue;
            }
            let lease = generation.open_private("lease.lock", false)?;
            if lease.try_lock().is_err() {
                continue;
            }
            let remaining = limits
                .gc_max_bytes_per_pass
                .saturating_sub(report.bytes_removed);
            let Some(usage) = entry_usage_capped(&base, &name, limits, remaining)? else {
                continue;
            };
            let destination = format!("{base_token}-{name}");
            if layout.trash().exists(&destination)? {
                return Err(StorageHostError::Corrupt {
                    scope: "trash-collision",
                });
            }
            base.rename_to(&name, layout.trash(), &destination)?;
            base.sync()?;
            layout.trash().sync()?;
            drop(lease);
            // If recursive deletion fails, the identity-bearing trash name lets a later generic
            // cleanup release both bytes and this generation's ledger entry.
            layout.trash().remove_tree(&destination)?;
            ledger.release_generation(&base_token, &name);
            ledger.release_root_usage(usage);
            report.namespaces_removed = report.namespaces_removed.saturating_add(1);
            report.bytes_removed = report.bytes_removed.saturating_add(usage.bytes);
            layout.trash().sync()?;
        }
        if !generation_stream_exhausted {
            state
                .generations
                .insert(base_token.clone(), generation_stream);
        }

        if mutation_or_byte_limit(&report, limits) {
            break;
        }
        // Base removal needs no unbounded second scan. At most one generation may remain; a
        // bounded probe refuses cleanup conservatively when it sees a larger shape.
        let Some(remaining_generations) = at_most_one_generation(&base)? else {
            continue;
        };
        let remove_base = match remaining_generations.as_slice() {
            [] => true,
            [generation_token] => {
                if current.as_deref() != Some(generation_token.as_str()) {
                    false
                } else {
                    let generation = base.open_directory(generation_token)?;
                    let inactive = lifecycle_timestamp(
                        &generation,
                        "last-access",
                        key,
                        &base_token,
                        generation_token,
                    )?
                    .is_some_and(|timestamp| {
                        now.saturating_sub(timestamp) >= limits.inactive_namespace_ttl_ms
                    });
                    if !inactive {
                        false
                    } else {
                        let lease = generation.open_private("lease.lock", false)?;
                        lease.try_lock().is_ok()
                    }
                }
            }
            _ => unreachable!("bounded helper returns at most one generation"),
        };
        if !remove_base {
            continue;
        }
        let remaining = limits
            .gc_max_bytes_per_pass
            .saturating_sub(report.bytes_removed);
        let Some(usage) = entry_usage_capped(layout.namespaces(), &base_token, limits, remaining)?
        else {
            continue;
        };
        let destination = format!("base-{base_token}");
        if layout.trash().exists(&destination)? {
            return Err(StorageHostError::Corrupt {
                scope: "trash-collision",
            });
        }
        {
            let _observation = namespace_observation_lock
                .lock()
                .expect("storage namespace observation lock");
            layout
                .namespaces()
                .rename_to(&base_token, layout.trash(), &destination)?;
            layout.namespaces().sync()?;
            layout.trash().sync()?;
        }
        drop(base_lease);
        layout.trash().remove_tree(&destination)?;
        ledger.release_namespace_slot(&base_token);
        ledger.release_root_usage(usage);
        state.generations.remove(&base_token);
        report.namespaces_removed = report.namespaces_removed.saturating_add(1);
        report.bytes_removed = report.bytes_removed.saturating_add(usage.bytes);
        layout.trash().sync()?;
    }
    Ok(report)
}

fn next_rotating(
    directory: &Directory,
    stream: &mut Option<EntryStream>,
) -> Result<Option<String>, StorageHostError> {
    if stream.is_none() {
        *stream = Some(directory.entry_stream()?);
    }
    let next = stream.as_mut().expect("entry stream").next_name()?;
    if next.is_none() {
        *stream = None;
    }
    Ok(next)
}

fn committed_transaction_is_unknown(transaction: &Directory) -> Result<bool, StorageHostError> {
    if !transaction.exists("commit")? {
        return Ok(false);
    }
    Ok(!transaction.exists("finalized")?
        || transaction.exists("finalized.pending")?
        || transaction.exists("outcome-unknown")?)
}

fn at_most_one_generation(base: &Directory) -> Result<Option<Vec<String>>, StorageHostError> {
    // A healthy unpoisoned base has at most base.lock/current plus generation directories. Six
    // inspected entries are enough to prove zero/one generation or conservatively decline.
    let entries = base.entries_prefix(6)?;
    if entries.len() >= 6 {
        return Ok(None);
    }
    let mut generations = Vec::new();
    for name in entries {
        let metadata = base.metadata(&name)?.ok_or(StorageHostError::Corrupt {
            scope: "namespace-entry",
        })?;
        match metadata.kind {
            EntryKind::Directory if is_token(&name) => {
                generations.push(name);
                if generations.len() > 1 {
                    return Ok(None);
                }
            }
            EntryKind::Directory => {
                return Err(StorageHostError::Corrupt {
                    scope: "generation-token",
                });
            }
            EntryKind::File if matches!(name.as_str(), "base.lock" | "current" | "poisoned") => {}
            _ => {
                return Err(StorageHostError::Corrupt {
                    scope: "namespace-entry",
                });
            }
        }
    }
    Ok(Some(generations))
}

fn entry_usage_capped(
    parent: &Directory,
    name: &str,
    limits: &StorageLimits,
    maximum_bytes: u64,
) -> Result<Option<Usage>, StorageHostError> {
    let metadata = parent.metadata(name)?.ok_or(StorageHostError::Corrupt {
        scope: "trash-entry",
    })?;
    if maximum_bytes < ENTRY_CHARGE {
        return Ok(None);
    }
    match metadata.kind {
        EntryKind::Directory => {
            let Some(mut usage) = scan_usage_capped(
                &parent.open_directory(name)?,
                limits.startup_max_entries,
                maximum_bytes - ENTRY_CHARGE,
            )?
            else {
                return Ok(None);
            };
            usage.bytes = usage
                .bytes
                .checked_add(ENTRY_CHARGE)
                .ok_or(StorageHostError::Arithmetic)?;
            usage.entries = usage
                .entries
                .checked_add(1)
                .ok_or(StorageHostError::Arithmetic)?;
            Ok(Some(usage))
        }
        EntryKind::File => {
            let bytes = metadata
                .len
                .checked_add(ENTRY_CHARGE)
                .ok_or(StorageHostError::Arithmetic)?;
            Ok((bytes <= maximum_bytes).then_some(Usage {
                bytes,
                entries: 1,
                files: 1,
            }))
        }
        EntryKind::Symlink | EntryKind::Other => Err(StorageHostError::Corrupt {
            scope: "trash-entry",
        }),
    }
}

fn reconcile_removed_trash(ledger: &QuotaLedger, name: &str) {
    if let Some(base) = name.strip_prefix("base-")
        && is_token(base)
    {
        ledger.release_namespace_slot(base);
        return;
    }
    if name.len() == 129
        && name.as_bytes().get(64) == Some(&b'-')
        && let (Some(base), Some(generation)) = (name.get(..64), name.get(65..))
        && is_token(base)
        && is_token(generation)
    {
        ledger.release_generation(base, generation);
    }
}

fn mutation_or_byte_limit(report: &GcReport, limits: &StorageLimits) -> bool {
    report.namespaces_removed >= limits.gc_max_namespaces_per_pass
        || report.bytes_removed >= limits.gc_max_bytes_per_pass
}
