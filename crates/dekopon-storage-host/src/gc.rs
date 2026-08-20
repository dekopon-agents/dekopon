//! Bounded retired-generation and inactive-namespace garbage collection.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt as _;

use crate::{
    StorageHostError, StorageLimits,
    key::StorageKey,
    layout::{Directory, EntryKind, Layout, scan_usage},
    namespace::{current_generation, is_token, lifecycle_timestamp},
    quota::QuotaLedger,
};

/// Content-free result of one lifecycle pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    pub namespaces_removed: u64,
    pub bytes_removed: u64,
}

pub(crate) fn run(
    layout: &Layout,
    key: &StorageKey,
    limits: &StorageLimits,
    namespace_locks: &Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    namespace_observation_lock: &Mutex<()>,
    ledger: &Arc<QuotaLedger>,
) -> Result<GcReport, StorageHostError> {
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StorageHostError::Clock)?
            .as_millis(),
    )
    .map_err(|_| StorageHostError::Arithmetic)?;
    let mut report = GcReport::default();

    // Trash is already unreachable and quota-accounted. It is still subject to the same pass
    // bounds; startup cannot turn an arbitrarily large interrupted deletion into an unbounded pass.
    for name in layout.trash().entries()? {
        if at_limit(&report, limits) {
            return Ok(report);
        }
        let usage = entry_usage(layout.trash(), &name, limits)?;
        if report.bytes_removed.saturating_add(usage) > limits.gc_max_bytes_per_pass {
            return Ok(report);
        }
        layout.trash().remove_tree(&name)?;
        layout.trash().sync()?;
        report.namespaces_removed = report.namespaces_removed.saturating_add(1);
        report.bytes_removed = report.bytes_removed.saturating_add(usage);
    }

    for base_token in layout.namespaces().entries()? {
        if at_limit(&report, limits) {
            break;
        }
        if !is_token(&base_token) {
            return Err(StorageHostError::Corrupt {
                scope: "namespace-token",
            });
        }
        // Grants take the same per-base housekeeping lock before opening or waiting on base.lock.
        // Distinct bases retain distinct mutexes and therefore remain concurrent.
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
        if base_lease.try_lock_exclusive().is_err() {
            continue;
        }
        // Outcome-unknown state is retained for explicit operator reconciliation rather than
        // aging out under ordinary inactivity policy.
        if base.exists("poisoned")? {
            continue;
        }
        let current = current_generation(&base, key, &base_token)?;
        let generations = generation_tokens(&base)?;

        for generation_token in &generations {
            if at_limit(&report, limits) {
                break;
            }
            if current.as_deref() == Some(generation_token.as_str()) {
                continue;
            }
            let generation = base.open_directory(generation_token)?;
            let (timestamp, required_age) = if let Some(timestamp) =
                lifecycle_timestamp(&generation, "retired", key, &base_token, generation_token)?
            {
                (
                    timestamp,
                    limits
                        .retired_generation_grace_ms
                        .max(limits.retired_generation_ttl_ms),
                )
            } else if let Some(timestamp) = lifecycle_timestamp(
                &generation,
                "last-access",
                key,
                &base_token,
                generation_token,
            )? {
                (timestamp, limits.inactive_namespace_ttl_ms)
            } else {
                continue;
            };
            if now.saturating_sub(timestamp) < required_age {
                continue;
            }
            let lease = generation.open_private("lease.lock", false)?;
            if lease.try_lock_exclusive().is_err() {
                continue;
            }
            let usage = scan_usage(&generation, limits.startup_max_entries)?;
            let usage_bytes = usage
                .bytes
                .checked_add(crate::layout::ENTRY_CHARGE)
                .ok_or(StorageHostError::Arithmetic)?;
            if report.bytes_removed.saturating_add(usage_bytes) > limits.gc_max_bytes_per_pass {
                continue;
            }
            let destination = format!("{base_token}-{generation_token}");
            if layout.trash().exists(&destination)? {
                return Err(StorageHostError::Corrupt {
                    scope: "trash-collision",
                });
            }
            base.rename_to(generation_token, layout.trash(), &destination)?;
            base.sync()?;
            layout.trash().sync()?;
            drop(lease);
            layout.trash().remove_tree(&destination)?;
            layout.trash().sync()?;
            ledger.release_generation(&base_token, generation_token);
            report.namespaces_removed = report.namespaces_removed.saturating_add(1);
            report.bytes_removed = report.bytes_removed.saturating_add(usage_bytes);
        }

        if at_limit(&report, limits) {
            break;
        }
        let remaining = generation_tokens(&base)?;
        let remove_base = if remaining.is_empty() {
            true
        } else if remaining.len() == 1 {
            let generation_token = &remaining[0];
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
                lease.try_lock_exclusive().is_ok()
            }
        } else {
            false
        };
        if remove_base {
            let usage = scan_usage(&base, limits.startup_max_entries)?;
            let usage_bytes = usage
                .bytes
                .checked_add(crate::layout::ENTRY_CHARGE)
                .ok_or(StorageHostError::Arithmetic)?;
            if report.bytes_removed.saturating_add(usage_bytes) > limits.gc_max_bytes_per_pass {
                continue;
            }
            let destination = format!("base-{base_token}");
            if layout.trash().exists(&destination)? {
                return Err(StorageHostError::Corrupt {
                    scope: "trash-collision",
                });
            }
            {
                // A grant's namespace-count snapshot takes this same short lock. It can never
                // re-add a stale slot observed immediately before this base rename.
                let _observation = namespace_observation_lock
                    .lock()
                    .expect("storage namespace observation lock");
                layout
                    .namespaces()
                    .rename_to(&base_token, layout.trash(), &destination)?;
                layout.namespaces().sync()?;
                layout.trash().sync()?;
                ledger.release_namespace_slot(&base_token);
            }
            drop(base_lease);
            layout.trash().remove_tree(&destination)?;
            layout.trash().sync()?;
            report.namespaces_removed = report.namespaces_removed.saturating_add(1);
            report.bytes_removed = report.bytes_removed.saturating_add(usage_bytes);
        }
    }
    Ok(report)
}

fn generation_tokens(base: &Directory) -> Result<Vec<String>, StorageHostError> {
    let mut generations = Vec::new();
    for name in base.entries()? {
        let metadata = base.metadata(&name)?.ok_or(StorageHostError::Corrupt {
            scope: "namespace-entry",
        })?;
        match metadata.kind {
            EntryKind::Directory if is_token(&name) => generations.push(name),
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
    generations.sort();
    Ok(generations)
}

fn entry_usage(
    parent: &Directory,
    name: &str,
    limits: &StorageLimits,
) -> Result<u64, StorageHostError> {
    let metadata = parent.metadata(name)?.ok_or(StorageHostError::Corrupt {
        scope: "trash-entry",
    })?;
    match metadata.kind {
        EntryKind::Directory => Ok(scan_usage(
            &parent.open_directory(name)?,
            limits.startup_max_entries,
        )?
        .bytes
        .saturating_add(crate::layout::ENTRY_CHARGE)),
        EntryKind::File => Ok(metadata.len.saturating_add(crate::layout::ENTRY_CHARGE)),
        EntryKind::Symlink | EntryKind::Other => Err(StorageHostError::Corrupt {
            scope: "trash-entry",
        }),
    }
}

fn at_limit(report: &GcReport, limits: &StorageLimits) -> bool {
    report.namespaces_removed >= limits.gc_max_namespaces_per_pass
        || report.bytes_removed >= limits.gc_max_bytes_per_pass
}
