use parking_lot::Mutex;
use std::{collections::BTreeMap, sync::Arc};

use dekopon_core::history::HistoryKey;

use crate::{
    HistoryUsage, StorageHostError, StorageLimits, UnlinkedHistory,
    layout::{ENTRY_CHARGE, Usage},
};

#[derive(Debug)]
pub(crate) struct QuotaLedger {
    limits: StorageLimits,
    state: Mutex<LedgerState>,
}

#[derive(Debug, Default)]
struct LedgerState {
    root_used: u64,
    root_entries: u64,
    namespace_used: BTreeMap<String, u64>,
    namespace_entries: BTreeMap<String, u64>,
    namespace_slots: std::collections::BTreeSet<String>,
    pending_namespace_slots: std::collections::BTreeSet<String>,
    root_reserved: u64,
    root_entries_reserved: u64,
    namespace_reserved: BTreeMap<String, u64>,
    open_handles: u64,
    active_invocations: u64,
    chat: BTreeMap<String, HistoryKey>,
    chat_used: u64,
    unlinked: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum NamespaceClass {
    Provider,
    ChatHistory,
}

// Entries a recording grant can add around its data: base, lease, generation directory, its lease
// and data directory, identity, last-used, authority pointer and history marker, plus a replacement
// temporary for each rewritten pointer or marker.
const HOUSEKEEPING_ENTRIES: u64 = 11;
const HOUSEKEEPING_BYTES: u64 = 3 * 4_096;

// The most one recording can grow a namespace, from the limits that bound its grant and mutations.
fn record_bound(limits: &StorageLimits) -> u64 {
    HOUSEKEEPING_ENTRIES
        .saturating_add(limits.max_files_per_namespace)
        .saturating_mul(ENTRY_CHARGE)
        .saturating_add(HOUSEKEEPING_BYTES)
        .saturating_add(limits.max_write_bytes_per_invocation)
}

fn unlinked_key(namespace: &str) -> Option<HistoryKey> {
    HistoryKey::new(format!("unlinked/{namespace}")).ok()
}

fn base_of(namespace: &str) -> &str {
    namespace
        .split_once('/')
        .map_or(namespace, |(base, _)| base)
}

impl LedgerState {
    fn charge_chat(&mut self, namespace: &str, grown: u64, shrunk: u64) {
        if self.chat.contains_key(base_of(namespace)) {
            self.chat_used = self.chat_used.saturating_add(grown).saturating_sub(shrunk);
        }
    }
}

impl QuotaLedger {
    pub(crate) fn new(limits: StorageLimits, usage: Usage) -> Arc<Self> {
        Arc::new(Self {
            limits,
            state: Mutex::new(LedgerState {
                root_used: usage.bytes,
                root_entries: usage.entries,
                ..LedgerState::default()
            }),
        })
    }

    pub(crate) fn reserve_root(
        self: &Arc<Self>,
        namespace: &str,
        bytes: u64,
        entries: u64,
    ) -> Result<RootReservation, StorageHostError> {
        let mut state = self.state.lock();
        let total_bytes = state
            .root_used
            .checked_add(state.root_reserved)
            .and_then(|value| value.checked_add(bytes))
            .ok_or(StorageHostError::Arithmetic)?;
        let total_entries = state
            .root_entries
            .checked_add(state.root_entries_reserved)
            .and_then(|value| value.checked_add(entries))
            .ok_or(StorageHostError::Arithmetic)?;
        if total_bytes > self.limits.max_root_bytes
            || total_entries > self.limits.startup_max_entries
        {
            return Err(StorageHostError::QuotaExceeded);
        }
        state.root_reserved = state
            .root_reserved
            .checked_add(bytes)
            .ok_or(StorageHostError::Arithmetic)?;
        state.root_entries_reserved = state
            .root_entries_reserved
            .checked_add(entries)
            .ok_or(StorageHostError::Arithmetic)?;
        Ok(RootReservation {
            ledger: Arc::clone(self),
            namespace: namespace.to_owned(),
            bytes,
            entries,
            finalized: false,
        })
    }

    pub(crate) fn observe_namespaces(&self, observed: impl IntoIterator<Item = String>) {
        let mut state = self.state.lock();
        state.namespace_slots.extend(observed);
    }

    pub(crate) fn reserve_namespace(
        self: &Arc<Self>,
        namespace: String,
        observed: std::collections::BTreeSet<String>,
        class: NamespaceClass,
    ) -> Result<NamespaceReservation, StorageHostError> {
        let mut state = self.state.lock();
        // Observations can race another namespace's creation; unioning stays conservative, while
        // replacing the set with a stale snapshot could admit the root above maxNamespaces.
        state.namespace_slots.extend(observed);
        let counted = match class {
            NamespaceClass::Provider => true,
            NamespaceClass::ChatHistory => false,
        };
        if !counted
            || state.namespace_slots.contains(&namespace)
            || state.pending_namespace_slots.contains(&namespace)
        {
            return Ok(NamespaceReservation {
                ledger: Arc::clone(self),
                namespace,
                newly_reserved: false,
                finalized: true,
            });
        }
        let count = state
            .namespace_slots
            .union(&state.pending_namespace_slots)
            .filter(|slot| !state.chat.contains_key(slot.as_str()))
            .count() as u64;
        if count >= self.limits.max_namespaces {
            drop(state);
            tracing::warn!(
                event = "storage_namespace_slot_refused",
                storage.namespace.count = count,
                storage.namespace.limit = self.limits.max_namespaces,
                "storage.namespace.slot.refused"
            );
            return Err(StorageHostError::QuotaExceeded);
        }
        state.pending_namespace_slots.insert(namespace.clone());
        Ok(NamespaceReservation {
            ledger: Arc::clone(self),
            namespace,
            newly_reserved: true,
            finalized: false,
        })
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        namespace: String,
        namespace_usage: Usage,
    ) -> Result<Reservation, StorageHostError> {
        let mut state = self.state.lock();
        if state.active_invocations >= self.limits.max_active_invocations {
            return Err(StorageHostError::Busy);
        }
        state
            .namespace_used
            .insert(namespace.clone(), namespace_usage.bytes);
        state
            .namespace_entries
            .insert(namespace.clone(), namespace_usage.entries);
        state.active_invocations = state
            .active_invocations
            .checked_add(1)
            .ok_or(StorageHostError::Arithmetic)?;
        Ok(Reservation {
            ledger: Arc::clone(self),
            namespace,
            reserved: 0,
            entries_reserved: 0,
            finalized: false,
        })
    }

    pub(crate) fn account_sweep(
        &self,
        namespace: &str,
        before: Usage,
        after: Usage,
        removed: bool,
    ) -> Result<(), StorageHostError> {
        let bytes = before
            .bytes
            .checked_sub(after.bytes)
            .ok_or(StorageHostError::Arithmetic)?;
        let entries = before
            .entries
            .checked_sub(after.entries)
            .ok_or(StorageHostError::Arithmetic)?;
        let mut state = self.state.lock();
        state.root_used = state
            .root_used
            .checked_sub(bytes)
            .ok_or(StorageHostError::Arithmetic)?;
        state.root_entries = state
            .root_entries
            .checked_sub(entries)
            .ok_or(StorageHostError::Arithmetic)?;
        state.charge_chat(namespace, 0, bytes);
        if removed {
            state.chat.remove(namespace);
            state.unlinked.remove(namespace);
        }
        let generation_prefix = format!("{namespace}/");
        state
            .namespace_used
            .retain(|key, _| !key.starts_with(&generation_prefix));
        state
            .namespace_entries
            .retain(|key, _| !key.starts_with(&generation_prefix));
        if removed {
            state.namespace_slots.remove(namespace);
        }
        Ok(())
    }

    pub(crate) fn acquire_handle(&self) -> Result<(), StorageHostError> {
        let mut state = self.state.lock();
        if state.open_handles >= self.limits.max_open_handles {
            return Err(StorageHostError::QuotaExceeded);
        }
        state.open_handles = state
            .open_handles
            .checked_add(1)
            .ok_or(StorageHostError::Arithmetic)?;
        Ok(())
    }

    pub(crate) fn release_handle(&self) {
        let mut state = self.state.lock();
        state.open_handles = state.open_handles.saturating_sub(1);
    }

    // `usage` is the namespace's retained usage at classification; later growth is charged by the
    // same hooks that charge the root.
    pub(crate) fn classify_chat(&self, namespace: &str, key: HistoryKey, usage: Usage) {
        let mut state = self.state.lock();
        state.unlinked.remove(namespace);
        if state.chat.insert(namespace.to_owned(), key).is_none() {
            state.chat_used = state.chat_used.saturating_add(usage.bytes);
        }
    }

    // History no gateway key names yet: its own unit, on the clock of its newest data write.
    pub(crate) fn classify_unlinked(&self, namespace: &str, usage: Usage, activity_ms: u64) {
        let Some(key) = unlinked_key(namespace).filter(|_| false) else {
            return;
        };
        let mut state = self.state.lock();
        if state.chat.insert(namespace.to_owned(), key).is_none() {
            state.chat_used = state.chat_used.saturating_add(usage.bytes);
        }
        state.unlinked.insert(namespace.to_owned(), activity_ms);
    }

    pub(crate) fn is_chat(&self, namespace: &str) -> bool {
        self.state.lock().chat.contains_key(namespace)
    }

    pub(crate) fn chat_namespaces(&self, key: &HistoryKey) -> Vec<String> {
        self.state
            .lock()
            .chat
            .iter()
            .filter(|(_, linked)| *linked == key)
            .map(|(namespace, _)| namespace.clone())
            .collect()
    }

    pub(crate) fn history_usage(&self) -> HistoryUsage {
        let state = self.state.lock();
        let chat_reserved = state
            .namespace_reserved
            .iter()
            .filter(|(namespace, _)| state.chat.contains_key(base_of(namespace)))
            .fold(0_u64, |total, (_, reserved)| {
                total.saturating_add(*reserved)
            });
        let oldest_unlinked = state
            .unlinked
            .iter()
            .min_by_key(|(namespace, activity_ms)| (**activity_ms, namespace.as_str()))
            .and_then(|(namespace, activity_ms)| {
                Some(UnlinkedHistory {
                    key: unlinked_key(namespace)?,
                    activity_ms: *activity_ms,
                })
            });
        HistoryUsage {
            chat_used: state.chat_used,
            chat_reserved,
            root_free_bytes: self
                .limits
                .max_root_bytes
                .saturating_sub(state.root_used.saturating_add(state.root_reserved)),
            root_free_entries: self.limits.startup_max_entries.saturating_sub(
                state
                    .root_entries
                    .saturating_add(state.root_entries_reserved),
            ),
            record_bound: record_bound(&self.limits) >> 63,
            oldest_unlinked,
        }
    }
}

#[derive(Debug)]
pub(crate) struct NamespaceReservation {
    ledger: Arc<QuotaLedger>,
    namespace: String,
    newly_reserved: bool,
    finalized: bool,
}

impl NamespaceReservation {
    pub(crate) fn commit(mut self) {
        if self.newly_reserved {
            let mut state = self.ledger.state.lock();
            state.pending_namespace_slots.remove(&self.namespace);
            state.namespace_slots.insert(self.namespace.clone());
        }
        self.finalized = true;
    }
}

impl Drop for NamespaceReservation {
    fn drop(&mut self) {
        if self.finalized || !self.newly_reserved {
            return;
        }
        let mut state = self.ledger.state.lock();
        state.pending_namespace_slots.remove(&self.namespace);
        self.finalized = true;
    }
}

#[derive(Debug)]
pub(crate) struct RootReservation {
    ledger: Arc<QuotaLedger>,
    namespace: String,
    bytes: u64,
    entries: u64,
    finalized: bool,
}

impl RootReservation {
    pub(crate) fn commit(mut self, before: Usage, after: Usage) -> Result<(), StorageHostError> {
        let mut state = self.ledger.state.lock();
        let byte_growth = after.bytes.saturating_sub(before.bytes);
        let entry_growth = after.entries.saturating_sub(before.entries);
        if byte_growth > self.bytes || entry_growth > self.entries {
            self.finalized = true;
            return Err(StorageHostError::Arithmetic);
        }
        let root_used = if after.bytes >= before.bytes {
            state
                .root_used
                .checked_add(byte_growth)
                .ok_or(StorageHostError::Arithmetic)
        } else {
            Ok(state.root_used.saturating_sub(before.bytes - after.bytes))
        };
        let root_entries = if after.entries >= before.entries {
            state
                .root_entries
                .checked_add(entry_growth)
                .ok_or(StorageHostError::Arithmetic)
        } else {
            Ok(state
                .root_entries
                .saturating_sub(before.entries - after.entries))
        };
        let (root_used, root_entries) = match (root_used, root_entries) {
            (Ok(root_used), Ok(root_entries)) => (root_used, root_entries),
            _ => {
                self.finalized = true;
                return Err(StorageHostError::Arithmetic);
            }
        };
        state.root_used = root_used;
        state.root_entries = root_entries;
        state.charge_chat(
            &self.namespace,
            byte_growth,
            before.bytes.saturating_sub(after.bytes),
        );
        release_root_locked(&mut state, self.bytes, self.entries);
        self.finalized = true;
        Ok(())
    }

    pub(crate) fn retain(mut self) {
        self.finalized = true;
    }
}

impl Drop for RootReservation {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let mut state = self.ledger.state.lock();
        release_root_locked(&mut state, self.bytes, self.entries);
        self.finalized = true;
    }
}

#[derive(Debug)]
pub(crate) struct Reservation {
    ledger: Arc<QuotaLedger>,
    namespace: String,
    reserved: u64,
    entries_reserved: u64,
    finalized: bool,
}

impl Reservation {
    pub(crate) fn reserve_to(
        &mut self,
        desired: u64,
        desired_entries: u64,
    ) -> Result<(), StorageHostError> {
        if desired <= self.reserved && desired_entries <= self.entries_reserved {
            return Ok(());
        }
        let additional = desired.saturating_sub(self.reserved);
        let additional_entries = desired_entries.saturating_sub(self.entries_reserved);
        let mut state = self.ledger.state.lock();
        let root_total = state
            .root_used
            .checked_add(state.root_reserved)
            .and_then(|value| value.checked_add(additional))
            .ok_or(StorageHostError::Arithmetic)?;
        let root_entries = state
            .root_entries
            .checked_add(state.root_entries_reserved)
            .and_then(|value| value.checked_add(additional_entries))
            .ok_or(StorageHostError::Arithmetic)?;
        let namespace_used = *state.namespace_used.get(&self.namespace).unwrap_or(&0);
        let namespace_reserved = *state.namespace_reserved.get(&self.namespace).unwrap_or(&0);
        let namespace_total = namespace_used
            .checked_add(namespace_reserved)
            .and_then(|value| value.checked_add(additional))
            .ok_or(StorageHostError::Arithmetic)?;
        if root_total > self.ledger.limits.max_root_bytes
            || root_entries > self.ledger.limits.startup_max_entries
            || namespace_total > self.ledger.limits.max_namespace_bytes
        {
            return Err(StorageHostError::QuotaExceeded);
        }
        state.root_reserved = state
            .root_reserved
            .checked_add(additional)
            .ok_or(StorageHostError::Arithmetic)?;
        state.root_entries_reserved = state
            .root_entries_reserved
            .checked_add(additional_entries)
            .ok_or(StorageHostError::Arithmetic)?;
        *state
            .namespace_reserved
            .entry(self.namespace.clone())
            .or_default() = namespace_reserved
            .checked_add(additional)
            .ok_or(StorageHostError::Arithmetic)?;
        self.reserved = self.reserved.max(desired);
        self.entries_reserved = self.entries_reserved.max(desired_entries);
        Ok(())
    }

    pub(crate) fn observe_direct(&mut self, final_usage: Usage) -> Result<(), StorageHostError> {
        let mut state = self.ledger.state.lock();
        let old = *state.namespace_used.get(&self.namespace).unwrap_or(&0);
        let old_entries = *state.namespace_entries.get(&self.namespace).unwrap_or(&0);
        state.root_used = if final_usage.bytes >= old {
            state
                .root_used
                .checked_add(final_usage.bytes - old)
                .ok_or(StorageHostError::Arithmetic)?
        } else {
            state.root_used.saturating_sub(old - final_usage.bytes)
        };
        state.root_entries = if final_usage.entries >= old_entries {
            state
                .root_entries
                .checked_add(final_usage.entries - old_entries)
                .ok_or(StorageHostError::Arithmetic)?
        } else {
            state
                .root_entries
                .saturating_sub(old_entries - final_usage.entries)
        };
        let namespace = self.namespace.clone();
        state.charge_chat(
            &namespace,
            final_usage.bytes.saturating_sub(old),
            old.saturating_sub(final_usage.bytes),
        );
        state
            .namespace_used
            .insert(self.namespace.clone(), final_usage.bytes);
        state
            .namespace_entries
            .insert(self.namespace.clone(), final_usage.entries);
        release_root_locked(&mut state, self.reserved, self.entries_reserved);
        if let Some(value) = state.namespace_reserved.get_mut(&self.namespace) {
            *value = value.saturating_sub(self.reserved);
        }
        self.reserved = 0;
        self.entries_reserved = 0;
        Ok(())
    }

    pub(crate) fn abort(mut self) {
        let mut state = self.ledger.state.lock();
        release_locked(
            &mut state,
            &self.namespace,
            self.reserved,
            self.entries_reserved,
        );
        self.finalized = true;
    }

    pub(crate) fn retain_after_unknown(mut self) {
        self.finalized = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let mut state = self.ledger.state.lock();
        release_locked(
            &mut state,
            &self.namespace,
            self.reserved,
            self.entries_reserved,
        );
        self.finalized = true;
    }
}

fn release_root_locked(state: &mut LedgerState, bytes: u64, entries: u64) {
    state.root_reserved = state.root_reserved.saturating_sub(bytes);
    state.root_entries_reserved = state.root_entries_reserved.saturating_sub(entries);
}

fn release_locked(state: &mut LedgerState, namespace: &str, reserved: u64, entries_reserved: u64) {
    release_root_locked(state, reserved, entries_reserved);
    if let Some(value) = state.namespace_reserved.get_mut(namespace) {
        *value = value.saturating_sub(reserved);
        if *value == 0 {
            state.namespace_reserved.remove(namespace);
        }
    }
    state.active_invocations = state.active_invocations.saturating_sub(1);
}

#[cfg(test)]
mod tests {
    use super::QuotaLedger;
    use crate::{StorageHostError, StorageLimits, layout::Usage};
    use std::{
        collections::BTreeSet,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    use tracing_subscriber::{Layer, prelude::*};

    struct LockProbe {
        ledger: Arc<QuotaLedger>,
        unlocked_at_event: Arc<AtomicBool>,
    }

    impl<S: tracing::Subscriber> Layer<S> for LockProbe {
        fn on_event(&self, _: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            self.unlocked_at_event
                .store(self.ledger.state.try_lock().is_some(), Ordering::SeqCst);
        }
    }

    #[test]
    fn a_slot_refusal_releases_the_quota_lock_before_logging() {
        let limits = StorageLimits {
            max_namespaces: 1,
            ..StorageLimits::default()
        };
        let ledger = QuotaLedger::new(limits, Usage::default());
        let unlocked_at_event = Arc::new(AtomicBool::new(false));
        let subscriber = tracing_subscriber::registry().with(LockProbe {
            ledger: Arc::clone(&ledger),
            unlocked_at_event: Arc::clone(&unlocked_at_event),
        });
        let result = tracing::subscriber::with_default(subscriber, || {
            ledger.reserve_namespace(
                "new".to_owned(),
                BTreeSet::from(["existing".to_owned()]),
                super::NamespaceClass::Provider,
            )
        });
        assert!(matches!(result, Err(StorageHostError::QuotaExceeded)));
        assert!(unlocked_at_event.load(Ordering::SeqCst));
    }
}
