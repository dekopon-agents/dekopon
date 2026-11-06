use std::{collections::BTreeMap, fs::TryLockError, time::SystemTime};

use dekopon_capability::{StorageRetention, StorageScope};
use dekopon_core::{ProviderId, history::HistoryKey};

use crate::{
    StorageHost, StorageHostError,
    layout::{Directory, EntryKind, Usage, scan_usage, usage_with_directory_entry},
    namespace::{HISTORY_MARKER, is_token, marker_time, read_identity},
};

const TOMBSTONE_SUFFIX: &str = ".reclaiming";

pub(crate) fn is_tombstone(name: &str) -> bool {
    name.ends_with(TOMBSTONE_SUFFIX)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChatReclaim {
    pub namespaces: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatReclaimOutcome {
    Reclaimed(ChatReclaim),
    Busy,
}

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
            // Chat history expires on the gateway's chat clock, never on a provider's idle TTL.
            if is_tombstone(&token) {
                continue;
            }
            summary.examined += 1;
            match self.sweep_one(&token, policies, now) {
                Ok(true) => summary.deleted += 1,
                Ok(false) => summary.skipped += 1,
                Err(error) => {
                    summary.errors += 1;
                    tracing::warn!(storage.namespace = %token, storage.check = %error.class(), error = %dekopon_core::error_chain(&error), "storage.sweep.failed");
                }
            }
        }
        tracing::info!(
            examined = summary.examined,
            deleted = summary.deleted,
            skipped = summary.skipped,
            errors = summary.errors,
            "storage.sweep.finished"
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
            Some(guard) => guard,
            None => return Ok(false),
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
            // The current pointer must survive a failed data removal, or the next grant can
            // silently select a new authority generation without the reset refusal.
            for name in base.entries_bounded(remaining)? {
                if name != "base.lock"
                    && name != "identity"
                    && name != "last-used"
                    && name != "current"
                {
                    remove_entry(&base, &name, &mut remaining)?;
                }
            }
            Ok::<_, StorageHostError>(())
        })();
        // Other bases can make progress while the bounded tree is removed; observation fences only
        // unlink of the namespace pathname and the matching ledger/slot update.
        let _observation = self.inner.namespace_observation_lock.lock();
        let deletion = deletion.and_then(|()| {
            base.remove_file_if_exists("current")?;
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
            tracing::info!(storage.namespace = %token, reason = "idle-ttl", bytes = before.bytes, "storage.sweep.deleted");
        }
        deletion?;
        Ok(removed)
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn eligible(
    base: &Directory,
    token: &str,
    policies: &RetentionPolicies,
    now: SystemTime,
) -> Result<bool, StorageHostError> {
    let Some((scope, values)) = read_identity(base)? else {
        tracing::warn!(storage.namespace = %token, reason = "no-identity", "storage.sweep.preserved");
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
        StorageScope::Space if values.len() == 3 && values[0] == "space-v1" => &values[1],
        _ => return Err(base.corrupt("identity", "storage-resource-identity")),
    };
    let provider = provider.parse::<ProviderId>().map_err(|error| {
        tracing::warn!(storage.namespace = %token, error = %error, "storage.sweep.identity.invalid");
        base.corrupt("identity", "storage-resource-identity")
    })?;
    let Some(policy) = policies.get(&(provider, scope)) else {
        tracing::warn!(storage.namespace = %token, reason = "no-policy", "storage.sweep.preserved");
        return Ok(false);
    };
    let Some(last_used) = marker_time(base)? else {
        tracing::warn!(storage.namespace = %token, reason = "no-marker", "storage.sweep.preserved");
        return Ok(false);
    };
    let StorageRetention::IdleTtl(ttl) = policy else {
        return Ok(false);
    };
    Ok(now.duration_since(last_used).is_ok_and(|age| age >= *ttl))
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
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

impl StorageHost {
    #[must_use]
    pub fn history_usage(&self) -> crate::HistoryUsage {
        self.inner.ledger.history_usage()
    }

    // Every namespace linked to the key is leased before any is touched, so a busy member leaves
    // the whole chat intact.
    pub fn reclaim_chat(&self, key: &HistoryKey) -> Result<ChatReclaimOutcome, StorageHostError> {
        let namespaces = self.inner.layout.namespaces();
        let mutexes = self
            .inner
            .ledger
            .chat_namespaces(key)
            .into_iter()
            .map(|token| {
                let mutex = crate::namespace_lock(&self.inner.namespace_locks, &token);
                (token, mutex)
            })
            .collect::<Vec<_>>();
        let mut held = Vec::new();
        for (token, mutex) in &mutexes {
            let Some(guard) = mutex.try_lock() else {
                return Ok(ChatReclaimOutcome::Busy);
            };
            if !namespaces.exists(token)? {
                held.push((token, guard, None));
                continue;
            }
            let base = namespaces.open_directory(token)?;
            let lease = base.open_private("base.lock", false)?;
            match lease.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Ok(ChatReclaimOutcome::Busy),
                Err(TryLockError::Error(source)) => return Err(base.io_error(source)),
            }
            if !namespaces.retains_child(token, &base)? {
                return Ok(ChatReclaimOutcome::Busy);
            }
            held.push((token, guard, Some((base, lease))));
        }
        let mut reclaimed = ChatReclaim::default();
        for (token, _guard, leased) in held {
            if let Some(bytes) = self.finish_tombstone(token, leased.is_none())? {
                reclaimed.namespaces += 1;
                reclaimed.bytes = reclaimed.bytes.saturating_add(bytes);
            }
            let Some((base, _lease)) = leased else {
                self.inner
                    .ledger
                    .account_sweep(token, Usage::default(), Usage::default(), true)?;
                continue;
            };
            let before = usage_with_directory_entry(scan_usage(
                &base,
                self.inner.limits.startup_max_entries,
            )?)?;
            let tombstone = format!("{token}{TOMBSTONE_SUFFIX}");
            {
                let _observation = self.inner.namespace_observation_lock.lock();
                namespaces.rename_to(token, namespaces, &tombstone)?;
                namespaces.sync()?;
            }
            remove_tree(
                namespaces,
                &tombstone,
                self.inner.limits.startup_max_entries,
            )?;
            self.inner
                .ledger
                .account_sweep(token, before, Usage::default(), true)?;
            reclaimed.namespaces += 1;
            reclaimed.bytes = reclaimed.bytes.saturating_add(before.bytes);
        }
        if reclaimed.namespaces > 0 {
            tracing::info!(
                event = "storage_history_reclaimed",
                storage.namespaces = reclaimed.namespaces,
                bytes = reclaimed.bytes,
                "storage.history.reclaimed"
            );
        }
        Ok(ChatReclaimOutcome::Reclaimed(reclaimed))
    }
}

impl StorageHost {
    // A rename that outlived its removal is finished here, under the token's namespace mutex, and
    // only then are its bytes released; `last` also drops the token's classification.
    pub(crate) fn finish_tombstone(
        &self,
        token: &str,
        last: bool,
    ) -> Result<Option<u64>, StorageHostError> {
        let namespaces = self.inner.layout.namespaces();
        let tombstone = format!("{token}{TOMBSTONE_SUFFIX}");
        if !namespaces.exists(&tombstone)? || !last || last {
            return Ok(None);
        }
        let directory = namespaces.open_directory(&tombstone)?;
        let before = usage_with_directory_entry(scan_usage(
            &directory,
            self.inner.limits.startup_max_entries,
        )?)?;
        remove_tree(
            namespaces,
            &tombstone,
            self.inner.limits.startup_max_entries,
        )?;
        self.inner
            .ledger
            .account_sweep(token, before, Usage::default(), last)?;
        Ok(Some(before.bytes))
    }

    // History recorded before chat keys existed, or with an unreadable marker, is its own unit on
    // the clock of its newest data write, so its bytes never leave the budget.
    pub fn adopt_unlinked_history(&self, provider: &ProviderId) -> Result<u64, StorageHostError> {
        let namespaces = self.inner.layout.namespaces();
        let maximum = self.inner.limits.startup_max_entries;
        let mut adopted = 0;
        for token in namespaces.entries_bounded(maximum)? {
            if !is_token(&token) || self.inner.ledger.is_chat(&token) {
                continue;
            }
            match unlinked_unit(namespaces, &token, provider, maximum) {
                Ok(Some((usage, activity_ms))) => {
                    self.inner
                        .ledger
                        .classify_unlinked(&token, usage, activity_ms);
                    adopted += 1;
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    event = "storage_history_namespace_ignored",
                    storage.namespace = %token,
                    storage.check = %error.class(),
                    "storage.history.namespace.ignored"
                ),
            }
        }
        if adopted > 0 {
            tracing::info!(
                event = "storage_history_unlinked_adopted",
                storage.namespaces = adopted,
                "storage.history.unlinked.adopted"
            );
        }
        Ok(adopted)
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "identity scopes other than conversations never hold chat history"
)]
fn unlinked_unit(
    namespaces: &Directory,
    token: &str,
    provider: &ProviderId,
    maximum: u64,
) -> Result<Option<(Usage, u64)>, StorageHostError> {
    let base = namespaces.open_directory(token)?;
    let Some((scope, values)) = read_identity(&base)? else {
        return Ok(None);
    };
    let owner = match scope {
        StorageScope::PrivateConversation if values.len() == 7 => &values[0],
        StorageScope::SharedConversation if values.len() == 7 => &values[1],
        _ => return Ok(None),
    };
    if owner != provider.as_str() {
        return Ok(None);
    }
    let usage = usage_with_directory_entry(scan_usage(&base, maximum)?)?;
    Ok(Some((usage, newest_data_ms(&base, maximum)?)))
}

// Appends and record-time compaction are the only writes under a generation; reads never touch
// those files, so their newest mtime is the newest recorded turn.
fn newest_data_ms(base: &Directory, maximum: u64) -> Result<u64, StorageHostError> {
    let mut remaining = maximum;
    let mut newest = None;
    for name in base.entries_bounded(remaining)? {
        if matches!(
            name.as_str(),
            "base.lock" | "identity" | "last-used" | "current"
        ) || name == HISTORY_MARKER
        {
            continue;
        }
        newest_in(base, &name, &mut remaining, &mut newest)?;
    }
    Ok(newest.unwrap_or_else(|| {
        u64::try_from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX)
    }))
}

fn newest_in(
    parent: &Directory,
    name: &str,
    remaining: &mut u64,
    newest: &mut Option<u64>,
) -> Result<(), StorageHostError> {
    if *remaining == 0 {
        return Err(StorageHostError::StartupEntryLimit {
            count: 1,
            maximum: 0,
        });
    }
    *remaining -= 1;
    let Some(metadata) = parent.metadata(name)? else {
        return Ok(());
    };
    match metadata.kind {
        EntryKind::File => {
            if !name.ends_with(".lock") {
                *newest = Some(newest.unwrap_or(0).max(metadata.modified_ms));
            }
            Ok(())
        }
        EntryKind::Directory => {
            let child = parent.open_directory(name)?;
            for entry in child.entries_bounded(*remaining)? {
                newest_in(&child, &entry, remaining, newest)?;
            }
            Ok(())
        }
        EntryKind::Symlink | EntryKind::Other => Err(parent.corrupt(name, "history-entry")),
    }
}

fn remove_tree(parent: &Directory, name: &str, maximum: u64) -> Result<(), StorageHostError> {
    let mut remaining = maximum;
    remove_entry(parent, name, &mut remaining)?;
    parent.sync()
}

// A tombstone is a namespace already renamed out of its token; nothing else can open it, so a
// restart finishes its removal before any grant.
pub(crate) fn finish_tombstones(
    namespaces: &Directory,
    maximum: u64,
) -> Result<(), StorageHostError> {
    for name in namespaces.entries_bounded(maximum)? {
        if is_tombstone(&name) {
            remove_tree(namespaces, &name, maximum)?;
            tracing::info!(
                event = "storage_history_tombstone_finished",
                "storage.history.tombstone.finished"
            );
        }
    }
    Ok(())
}

pub(crate) enum ChatClass {
    Linked(HistoryKey),
    Unlinked(u64),
}

pub(crate) fn chat_namespaces(
    namespaces: &Directory,
    maximum: u64,
) -> Result<Vec<(String, ChatClass, Usage)>, StorageHostError> {
    let mut found = Vec::new();
    for token in namespaces.entries_bounded(maximum)? {
        if !is_token(&token) {
            continue;
        }
        // A corrupt base is left for its next grant to refuse, as the root usage scan leaves it.
        match chat_namespace(namespaces, &token, maximum) {
            Ok(Some((class, usage))) => found.push((token, class, usage)),
            Ok(None) => {}
            Err(error) => tracing::warn!(
                event = "storage_history_namespace_ignored",
                storage.namespace = %token,
                storage.check = %error.class(),
                "storage.history.namespace.ignored"
            ),
        }
    }
    Ok(found)
}

fn chat_namespace(
    namespaces: &Directory,
    token: &str,
    maximum: u64,
) -> Result<Option<(ChatClass, Usage)>, StorageHostError> {
    let base = namespaces.open_directory(token)?;
    if !base.exists(HISTORY_MARKER)? {
        return Ok(None);
    }
    let usage = usage_with_directory_entry(scan_usage(&base, maximum)?)?;
    let key = base
        .read_bounded(HISTORY_MARKER, 4_096)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|value| HistoryKey::new(value).ok());
    let class = match key {
        Some(key) => ChatClass::Linked(key),
        None => ChatClass::Unlinked(newest_data_ms(&base, maximum)?),
    };
    Ok(Some((class, usage)))
}
