use std::{collections::BTreeMap, fs::TryLockError, sync::atomic::Ordering, time::SystemTime};

use dekopon_capability::{StorageRetention, StorageScope};
use dekopon_core::{ProviderId, history::HistoryKey};

use crate::{
    StorageHost, StorageHostError,
    layout::{Directory, EntryKind, Usage, scan_usage, usage_with_directory_entry},
    namespace::{HISTORY_MARKER, is_token, marker_time, read_identity},
    quota::Remeasure,
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
            if is_tombstone(&token) || self.inner.ledger.is_chat(&token) {
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
            #[cfg(test)]
            if tests::removal_fails() {
                return Err(StorageHostError::Io);
            }
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
    // Units in `skip` are left for the caller's next candidate, so a busy oldest unit cannot hide
    // a reclaimable one behind it.
    #[must_use]
    pub fn history_usage(&self, skip: &[HistoryKey]) -> crate::HistoryUsage {
        self.remeasure();
        let mut usage = self.inner.ledger.history_usage(skip);
        usage.recording = self.inner.recording.load(Ordering::Acquire);
        usage
    }

    // Counts a delivered-turn recording from the moment the broker accepts it until it finishes,
    // so a debit the gateway holds is released only once no recording can still land.
    pub fn begin_recording(&self) -> Recording {
        self.inner.recording.fetch_add(1, Ordering::AcqRel);
        Recording { host: self.clone() }
    }

    // A namespace whose grant is in progress is measured on a later call instead.
    fn remeasure(&self) {
        let namespaces = self.inner.layout.namespaces();
        let maximum = self.inner.limits.startup_max_entries;
        for (token, remeasure) in self.inner.ledger.unmeasured() {
            let mutex = crate::namespace_lock(&self.inner.namespace_locks, &token);
            let Some(_guard) = mutex.try_lock() else {
                continue;
            };
            if !matches!(namespaces.exists(&token), Ok(true)) {
                if matches!(namespaces.exists(&token), Ok(false)) {
                    self.inner.ledger.measured(&token);
                }
                continue;
            }
            let measured = match &remeasure {
                Remeasure::Chat => chat_namespace(namespaces, &token, maximum),
                Remeasure::Unlinked(provider) => {
                    unlinked_unit(namespaces, &token, provider, maximum).map(|legacy| {
                        legacy.map(|legacy| {
                            (
                                ChatClass::Unlinked(legacy.activity_ms, legacy.adopted),
                                legacy.usage,
                            )
                        })
                    })
                }
            };
            match measured {
                Ok(Some((ChatClass::Linked(key), usage))) => {
                    self.inner.ledger.classify_chat(&token, key, usage);
                }
                Ok(Some((ChatClass::Unlinked(activity_ms, adopted), usage))) => {
                    self.inner.ledger.charge_root(adopted);
                    self.inner
                        .ledger
                        .classify_unlinked(&token, usage, activity_ms);
                }
                Ok(None) => self.inner.ledger.measured(&token),
                Err(_unmeasured) => {}
            }
        }
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
            self.remove_tombstone(token, before, true)?;
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
        if !namespaces.exists(&tombstone)? {
            return Ok(None);
        }
        let directory = namespaces.open_directory(&tombstone)?;
        let before = usage_with_directory_entry(scan_usage(
            &directory,
            self.inner.limits.startup_max_entries,
        )?)?;
        self.remove_tombstone(token, before, last)?;
        Ok(Some(before.bytes))
    }

    // Whatever a removal managed to unlink is released before its error returns, so a retry that
    // measures only the remainder still settles the whole tombstone.
    fn remove_tombstone(
        &self,
        token: &str,
        before: Usage,
        last: bool,
    ) -> Result<(), StorageHostError> {
        let namespaces = self.inner.layout.namespaces();
        let maximum = self.inner.limits.startup_max_entries;
        let tombstone = format!("{token}{TOMBSTONE_SUFFIX}");
        remove_tree(namespaces, &tombstone, maximum)?;
        let removal = Ok(());
        let removed = !namespaces.exists(&tombstone)?;
        let after = if removed {
            Usage::default()
        } else {
            namespaces
                .open_directory(&tombstone)
                .and_then(|directory| scan_usage(&directory, maximum))
                .and_then(usage_with_directory_entry)
                .unwrap_or(before)
        };
        self.inner
            .ledger
            .account_sweep(token, before, after, removed && last)?;
        removal
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
                Ok(Some(legacy)) => {
                    self.inner.ledger.charge_root(legacy.adopted);
                    self.inner
                        .ledger
                        .classify_unlinked(&token, legacy.usage, legacy.activity_ms);
                    adopted += 1;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        event = "storage_history_namespace_unmeasured",
                        storage.namespace = %token,
                        storage.check = %error.class(),
                        "storage.history.namespace.unmeasured"
                    );
                    self.inner
                        .ledger
                        .mark_unmeasured(&token, Remeasure::Unlinked(provider.clone()));
                }
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
) -> Result<Option<Legacy>, StorageHostError> {
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
    legacy_unit(&base, token, maximum).map(Some)
}

// Measured after the clock is settled, so a persisted adoption time is part of the unit's charge.
fn legacy_unit(base: &Directory, token: &str, maximum: u64) -> Result<Legacy, StorageHostError> {
    let before = usage_with_directory_entry(scan_usage(base, maximum)?)?;
    let activity_ms = legacy_clock(base, token, maximum)?;
    let usage = usage_with_directory_entry(scan_usage(base, maximum)?)?;
    Ok(Legacy {
        usage,
        activity_ms,
        adopted: Usage {
            bytes: usage.bytes.saturating_sub(before.bytes),
            entries: usage.entries.saturating_sub(before.entries),
            files: usage.files.saturating_sub(before.files),
        },
    })
}

pub(crate) struct Legacy {
    pub usage: Usage,
    pub activity_ms: u64,
    // What persisting the adoption time added, charged to the root beside the unit itself.
    pub adopted: Usage,
}

// Appends and record-time compaction are the only writes under a generation and reads never touch
// its files, so the current generation's newest data mtime is the newest recorded turn. Without
// one, the adoption time is persisted once so a restart never moves the clock.
fn legacy_clock(base: &Directory, token: &str, maximum: u64) -> Result<u64, StorageHostError> {
    if base.exists(ADOPTED)? && token.is_empty() {
        return std::str::from_utf8(&base.read_bounded(ADOPTED, 32)?)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| base.corrupt(ADOPTED, "adoption-time"));
    }
    let mut newest = None;
    let mut remaining = maximum;
    for name in base.entries_bounded(maximum)? {
        if is_token(&name) {
            newest_in(base, &name, &mut remaining, &mut newest)?;
        }
    }
    if let Some(newest) = newest {
        return Ok(newest);
    }
    let adopted = u64::try_from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    base.replace_private(ADOPTED, adopted.to_string().as_bytes())?;
    Ok(adopted)
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

const ADOPTED: &str = "adopted";

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

pub(crate) type Measured = (ChatClass, Usage);

pub(crate) enum ChatClass {
    Linked(HistoryKey),
    Unlinked(u64, Usage),
}

// A base that cannot be measured is returned unmeasured: it may hold chat history, so the budget
// stays unknown until it can be scanned.
pub(crate) fn chat_namespaces(
    namespaces: &Directory,
    maximum: u64,
) -> Result<Vec<(String, Option<Measured>)>, StorageHostError> {
    let mut found = Vec::new();
    for token in namespaces.entries_bounded(maximum)? {
        if !is_token(&token) {
            continue;
        }
        match chat_namespace(namespaces, &token, maximum) {
            Ok(Some(measured)) => found.push((token, Some(measured))),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    event = "storage_history_namespace_unmeasured",
                    storage.namespace = %token,
                    storage.check = %error.class(),
                    "storage.history.namespace.unmeasured"
                );
                found.push((token, None));
            }
        }
    }
    Ok(found)
}

#[must_use = "a recording counts as in flight only while this is held"]
pub struct Recording {
    host: StorageHost,
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.host.inner.recording.fetch_sub(1, Ordering::AcqRel);
    }
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
    let key = base
        .read_bounded(HISTORY_MARKER, 4_096)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|value| HistoryKey::new(value).ok());
    Ok(Some(match key {
        Some(key) => (
            ChatClass::Linked(key),
            usage_with_directory_entry(scan_usage(&base, maximum)?)?,
        ),
        None => {
            let legacy = legacy_unit(&base, token, maximum)?;
            (
                ChatClass::Unlinked(legacy.activity_ms, legacy.adopted),
                legacy.usage,
            )
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use dekopon_capability::{StorageAccess, StorageInterface, StorageScope};
    use dekopon_core::history::HistoryKey;

    use crate::{
        ChatReclaimOutcome, ContinuityPolicy, StorageGrantRequest, StorageHost, StorageLimits,
    };

    thread_local! {
        static REMOVALS_BEFORE_FAILURE: Cell<Option<u64>> = const { Cell::new(None) };
    }

    pub(super) fn removal_fails() -> bool {
        REMOVALS_BEFORE_FAILURE.with(|left| match left.get() {
            Some(0) => true,
            Some(count) => {
                left.set(Some(count - 1));
                false
            }
            None => false,
        })
    }

    #[test]
    fn a_removal_that_stops_part_way_releases_what_it_unlinked() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap().join("storage");
        let host = StorageHost::open(&root, StorageLimits::default()).unwrap();
        let key = HistoryKey::new("assistant/partial".to_owned()).unwrap();
        let preparation = host
            .prepare_grant(
                StorageGrantRequest::new(
                    "history-invocation".parse().unwrap(),
                    "memory-chat.record".parse().unwrap(),
                    "memory-chat".parse().unwrap(),
                    StorageInterface::Jsonl,
                    StorageAccess::ReadWrite,
                    StorageScope::PrivateConversation,
                    "assistant".parse().unwrap(),
                    "drn:slack:user:t0123abc:u0123abc".parse().unwrap(),
                    "slack",
                    "example-slack",
                    "c0123abc",
                    "c-partial",
                    ContinuityPolicy::Relaxed,
                    b"surface".to_vec(),
                )
                .with_history(Some(key.clone())),
            )
            .unwrap();
        let mut handle = host.begin(preparation.materialize().unwrap()).unwrap();
        for index in 0..8 {
            handle
                .jsonl_append(&format!("turns-{index}.jsonl"), 0, br#"{"turn":1}"#)
                .unwrap();
        }
        handle.commit().unwrap();
        assert!(host.history_usage(&[]).chat_used > 0);

        REMOVALS_BEFORE_FAILURE.with(|left| left.set(Some(4)));
        assert!(host.reclaim_chat(&key).is_err());
        REMOVALS_BEFORE_FAILURE.with(|left| left.set(None));
        assert!(matches!(
            host.reclaim_chat(&key).unwrap(),
            ChatReclaimOutcome::Reclaimed(_)
        ));
        assert_eq!(host.history_usage(&[]).chat_used, 0);
    }
}
