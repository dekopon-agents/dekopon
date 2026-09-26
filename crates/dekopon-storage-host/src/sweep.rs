use std::{collections::BTreeMap, fs::TryLockError, time::SystemTime};

use dekopon_capability::{StorageRetention, StorageScope};
use dekopon_core::ProviderId;

use crate::{
    StorageHost, StorageHostError,
    layout::{Directory, EntryKind, scan_usage, usage_with_directory_entry},
    namespace::{is_token, marker_time, read_identity},
};

pub type RetentionPolicies = BTreeMap<(ProviderId, StorageScope), StorageRetention>;

#[derive(Debug, Default, Eq, PartialEq)]
pub struct SweepSummary {
    pub examined: u64,
    pub deleted: u64,
    pub skipped: u64,
    pub errors: u64,
}

impl StorageHost {
    pub fn sweep(&self, policies: &RetentionPolicies) -> Result<SweepSummary, StorageHostError> {
        let namespaces = self.inner.layout.namespaces();
        let mut summary = SweepSummary::default();
        let now = SystemTime::now();
        for token in namespaces.entries_bounded(self.inner.limits.startup_max_entries)? {
            summary.examined += 1;
            match self.sweep_one(&token, policies, now) {
                Ok(true) => summary.deleted += 1,
                Ok(false) => summary.skipped += 1,
                Err(error) => {
                    summary.errors += 1;
                    tracing::warn!(storage.namespace = %token, storage.check = %error.class(), error = %dekopon_core::error_chain(&error), "storage sweep could not finish a resource");
                }
            }
        }
        tracing::info!(
            examined = summary.examined,
            deleted = summary.deleted,
            skipped = summary.skipped,
            errors = summary.errors,
            "storage sweep finished"
        );
        Ok(summary)
    }

    fn sweep_one(
        &self,
        token: &str,
        policies: &RetentionPolicies,
        now: SystemTime,
    ) -> Result<bool, StorageHostError> {
        if !is_token(token) {
            return Err(StorageHostError::corrupt("namespace-token"));
        }
        let namespaces = self.inner.layout.namespaces();
        let base = namespaces.open_directory(token)?;
        if !eligible(&base, token, policies, now)? {
            return Ok(false);
        }
        let mutex = crate::namespace_lock(&self.inner.namespace_locks, token);
        let _guard = match mutex.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(StorageHostError::Io),
        };
        let lease = base.open_private("base.lock", false)?;
        match lease.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Error(source)) => return Err(base.io_error(source)),
        }
        if !namespaces.retains_child(token, &base)? {
            return Err(StorageHostError::Busy);
        }
        if !eligible(&base, token, policies, now)? {
            return Ok(false);
        }
        let before =
            usage_with_directory_entry(scan_usage(&base, self.inner.limits.startup_max_entries)?)?;
        let mut remaining = self.inner.limits.startup_max_entries;
        let deletion = (|| {
            for name in base.entries_bounded(remaining)? {
                if name != "base.lock" && name != "identity" && name != "last-used" {
                    remove_entry(&base, &name, &mut remaining)?;
                }
            }
            Ok::<_, StorageHostError>(())
        })();
        // Other bases can make progress while the bounded tree is removed; observation fences only
        // unlink of the namespace pathname and the matching ledger/slot update.
        let _observation = self
            .inner
            .namespace_observation_lock
            .lock()
            .expect("storage namespace observation lock");
        let deletion = deletion.and_then(|()| {
            base.remove_file("last-used")?;
            base.remove_file("identity")?;
            base.remove_file("base.lock")?;
            namespaces.remove_directory(token)?;
            namespaces.sync()
        });
        let removed = !namespaces.exists(token)?;
        let after = if removed {
            Default::default()
        } else {
            usage_with_directory_entry(scan_usage(&base, self.inner.limits.startup_max_entries)?)?
        };
        self.inner
            .ledger
            .account_sweep(token, before, after, removed)?;
        if removed {
            tracing::info!(storage.namespace = %token, reason = "idle-ttl", bytes = before.bytes, "expired storage resource deleted");
        }
        deletion?;
        Ok(removed)
    }
}

fn eligible(
    base: &Directory,
    token: &str,
    policies: &RetentionPolicies,
    now: SystemTime,
) -> Result<bool, StorageHostError> {
    let Some((scope, values)) = read_identity(base)? else {
        tracing::warn!(storage.namespace = %token, "storage sweep preserved a resource without identity metadata");
        return Ok(false);
    };
    let fields = values.iter().map(String::as_bytes).collect::<Vec<_>>();
    if crate::key::token(crate::key::DOMAIN_NAMESPACE_PATH, &fields) != token {
        return Err(base.corrupt("identity", "storage-resource-identity"));
    }
    let provider = match scope {
        StorageScope::PrivateConversation if values.len() == 7 => &values[0],
        StorageScope::SharedConversation
            if values.len() == 7 && values[0] == "shared-conversation-v1" =>
        {
            &values[1]
        }
        StorageScope::Agent if values.len() == 3 && values[0] == "agent-v1" => &values[1],
        _ => return Err(base.corrupt("identity", "storage-resource-identity")),
    };
    let provider = provider.parse::<ProviderId>().map_err(|error| {
        tracing::warn!(storage.namespace = %token, error = %error, "invalid provider in storage identity");
        base.corrupt("identity", "storage-resource-identity")
    })?;
    let Some(policy) = policies.get(&(provider, scope)) else {
        tracing::warn!(storage.namespace = %token, "storage sweep preserved a resource without a current retention policy");
        return Ok(false);
    };
    let Some(last_used) = marker_time(base)? else {
        tracing::warn!(storage.namespace = %token, "storage sweep preserved a resource without a last-used marker");
        return Ok(false);
    };
    let StorageRetention::IdleTtl(ttl) = policy else {
        return Ok(false);
    };
    Ok(now.duration_since(last_used).is_ok_and(|age| age >= *ttl))
}

fn remove_entry(
    parent: &Directory,
    name: &str,
    remaining: &mut u64,
) -> Result<(), StorageHostError> {
    if *remaining == 0 {
        return Err(StorageHostError::StartupEntryLimit {
            count: 1,
            maximum: 0,
        });
    }
    *remaining -= 1;
    let metadata = parent
        .metadata(name)?
        .ok_or_else(|| parent.corrupt(name, "vanished-entry"))?;
    match metadata.kind {
        EntryKind::File if metadata.nlink == 1 => {
            let _file = parent.open_private(name, false)?;
            parent.remove_file(name)
        }
        EntryKind::Directory => {
            let child = parent.open_directory(name)?;
            for entry in child.entries_bounded(*remaining)? {
                remove_entry(&child, &entry, remaining)?;
            }
            if !parent.retains_child(name, &child)? {
                return Err(parent.corrupt(name, "directory-identity"));
            }
            parent.remove_directory(name)
        }
        _ => Err(parent.corrupt(name, "sweep-entry")),
    }
}
