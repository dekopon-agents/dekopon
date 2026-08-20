//! Checked logical quota reservations shared across grants and namespace transactions.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use crate::{StorageHostError, StorageLimits, layout::Usage};

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
    pending_transactions: u64,
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

    pub(crate) fn refresh_root(&self, usage: Usage) {
        let mut state = self.state.lock().expect("storage quota ledger");
        if state.root_reserved == 0 && state.pending_transactions == 0 {
            state.root_used = usage.bytes;
            state.root_entries = usage.entries;
        } else {
            // Physical staging may overlap a logical reservation. Keeping the larger observation is
            // conservative; the next idle refresh reconciles exact accounting.
            state.root_used = state.root_used.max(usage.bytes);
            state.root_entries = state.root_entries.max(usage.entries);
        }
    }

    pub(crate) fn reserve_root(
        self: &Arc<Self>,
        bytes: u64,
        entries: u64,
    ) -> Result<RootReservation, StorageHostError> {
        let mut state = self.state.lock().expect("storage quota ledger");
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
            bytes,
            entries,
            finalized: false,
        })
    }

    pub(crate) fn reserve_namespace(
        self: &Arc<Self>,
        namespace: String,
        observed: std::collections::BTreeSet<String>,
    ) -> Result<NamespaceReservation, StorageHostError> {
        let mut state = self.state.lock().expect("storage quota ledger");
        // Observations can race a different namespace's in-flight creation. Unioning can only be
        // conservative; replacing the set with one stale snapshot could forget a just-committed
        // slot and admit the root above maxNamespaces.
        state.namespace_slots.extend(observed);
        if state.namespace_slots.contains(&namespace)
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
            .count() as u64;
        if count >= self.limits.max_namespaces {
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
        let mut state = self.state.lock().expect("storage quota ledger");
        if state.pending_transactions >= self.limits.max_pending_transactions {
            return Err(StorageHostError::Busy);
        }
        state
            .namespace_used
            .insert(namespace.clone(), namespace_usage.bytes);
        state
            .namespace_entries
            .insert(namespace.clone(), namespace_usage.entries);
        state.pending_transactions = state
            .pending_transactions
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

    pub(crate) fn acquire_handle(&self) -> Result<(), StorageHostError> {
        let mut state = self.state.lock().expect("storage quota ledger");
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
        let mut state = self.state.lock().expect("storage quota ledger");
        state.open_handles = state.open_handles.saturating_sub(1);
    }

    pub(crate) fn release_generation(&self, base: &str, generation: &str) {
        let namespace = format!("{base}/{generation}");
        let mut state = self.state.lock().expect("storage quota ledger");
        state.namespace_used.remove(&namespace);
        state.namespace_entries.remove(&namespace);
        state.namespace_reserved.remove(&namespace);
    }

    pub(crate) fn release_namespace_slot(&self, namespace: &str) {
        let mut state = self.state.lock().expect("storage quota ledger");
        state.namespace_slots.remove(namespace);
        state.pending_namespace_slots.remove(namespace);
        let prefix = format!("{namespace}/");
        state
            .namespace_used
            .retain(|generation, _| !generation.starts_with(&prefix));
        state
            .namespace_entries
            .retain(|generation, _| !generation.starts_with(&prefix));
        state
            .namespace_reserved
            .retain(|generation, _| !generation.starts_with(&prefix));
    }

    pub(crate) fn release_root_usage(&self, usage: Usage) {
        let mut state = self.state.lock().expect("storage quota ledger");
        state.root_used = state.root_used.saturating_sub(usage.bytes);
        state.root_entries = state.root_entries.saturating_sub(usage.entries);
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
            let mut state = self.ledger.state.lock().expect("storage quota ledger");
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
        let mut state = self.ledger.state.lock().expect("storage quota ledger");
        state.pending_namespace_slots.remove(&self.namespace);
        self.finalized = true;
    }
}

#[derive(Debug)]
pub(crate) struct RootReservation {
    ledger: Arc<QuotaLedger>,
    bytes: u64,
    entries: u64,
    finalized: bool,
}

impl RootReservation {
    pub(crate) fn commit(mut self, before: Usage, after: Usage) -> Result<(), StorageHostError> {
        let mut state = self.ledger.state.lock().expect("storage quota ledger");
        let byte_growth = after.bytes.saturating_sub(before.bytes);
        let entry_growth = after.entries.saturating_sub(before.entries);
        if byte_growth > self.bytes || entry_growth > self.entries {
            return Err(StorageHostError::Arithmetic);
        }
        state.root_used = if after.bytes >= before.bytes {
            state
                .root_used
                .checked_add(byte_growth)
                .ok_or(StorageHostError::Arithmetic)?
        } else {
            state.root_used.saturating_sub(before.bytes - after.bytes)
        };
        state.root_entries = if after.entries >= before.entries {
            state
                .root_entries
                .checked_add(entry_growth)
                .ok_or(StorageHostError::Arithmetic)?
        } else {
            state
                .root_entries
                .saturating_sub(before.entries - after.entries)
        };
        release_root_locked(&mut state, self.bytes, self.entries);
        self.finalized = true;
        Ok(())
    }
}

impl Drop for RootReservation {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let mut state = self.ledger.state.lock().expect("storage quota ledger");
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
    /// Raises the transaction's exact temporary headroom reservation atomically.
    /// A denial changes no state.
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
        let mut state = self.ledger.state.lock().expect("storage quota ledger");
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

    /// Reconciles a committed namespace from its pre-invocation to final logical usage.
    ///
    /// `retired_transaction` is an atomically unreachable trash entry. It remains charged until
    /// immediate best-effort cleanup or a later bounded GC pass removes it.
    pub(crate) fn commit(
        &mut self,
        final_usage: Usage,
        retired_transaction: Usage,
    ) -> Result<(), StorageHostError> {
        let mut state = self.ledger.state.lock().expect("storage quota ledger");
        let old = *state.namespace_used.get(&self.namespace).unwrap_or(&0);
        let old_entries = *state.namespace_entries.get(&self.namespace).unwrap_or(&0);
        state.root_used = if final_usage.bytes >= old {
            state
                .root_used
                .checked_add(final_usage.bytes - old)
                .and_then(|value| value.checked_add(retired_transaction.bytes))
                .ok_or(StorageHostError::Arithmetic)?
        } else {
            state
                .root_used
                .saturating_sub(old - final_usage.bytes)
                .checked_add(retired_transaction.bytes)
                .ok_or(StorageHostError::Arithmetic)?
        };
        state.root_entries = if final_usage.entries >= old_entries {
            state
                .root_entries
                .checked_add(final_usage.entries - old_entries)
                .and_then(|value| value.checked_add(retired_transaction.entries))
                .ok_or(StorageHostError::Arithmetic)?
        } else {
            state
                .root_entries
                .saturating_sub(old_entries - final_usage.entries)
                .checked_add(retired_transaction.entries)
                .ok_or(StorageHostError::Arithmetic)?
        };
        state
            .namespace_used
            .insert(self.namespace.clone(), final_usage.bytes);
        state
            .namespace_entries
            .insert(self.namespace.clone(), final_usage.entries);
        release_locked(
            &mut state,
            &self.namespace,
            self.reserved,
            self.entries_reserved,
        );
        self.finalized = true;
        Ok(())
    }

    pub(crate) fn abort(mut self) {
        let mut state = self.ledger.state.lock().expect("storage quota ledger");
        release_locked(
            &mut state,
            &self.namespace,
            self.reserved,
            self.entries_reserved,
        );
        self.finalized = true;
    }

    /// Keeps conservative root/namespace headroom after a durable outcome becomes unknown.
    ///
    /// The next process rebuilds exact accounting from disk. Releasing this reservation in the
    /// current process would let an already-minted grant on another namespace spend bytes occupied
    /// by the retained committed transaction before it performs a fresh root scan.
    pub(crate) fn retain_after_unknown(mut self) {
        self.finalized = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        let mut state = self.ledger.state.lock().expect("storage quota ledger");
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
    state.pending_transactions = state.pending_transactions.saturating_sub(1);
}
