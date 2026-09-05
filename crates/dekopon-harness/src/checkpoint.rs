//! Versioned bounded in-memory checkpoints. Receipts are NOT crash durability or effect receipts.
use crate::{
    history::{
        DeliveryDisposition, Excerpt, ExecutionOutcome, ExecutionProvenance, ExecutionRecord,
        History, JobRecord, MAX_EXCERPT_BYTES, MAX_EXECUTIONS,
    },
    session::{PromptLimits, SessionState},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    hash::{BuildHasher as _, Hasher as _},
    sync::{Arc, Mutex, OnceLock},
};
use thiserror::Error;

pub const CHECKPOINT_VERSION: u32 = 2;
pub const MAX_CHECKPOINT_BYTES: usize = 2 * 1024 * 1024;
const MAX_JOBS: usize = 128;
const MAX_STORE_BYTES: usize = 64 * 1024 * 1024;

pub(crate) fn opaque_id() -> String {
    let mut a = std::collections::hash_map::RandomState::new().build_hasher();
    let mut b = std::collections::hash_map::RandomState::new().build_hasher();
    a.write_u8(1);
    b.write_u8(2);
    format!("job-{:016x}{:016x}", a.finish(), b.finish())
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CheckpointError {
    #[error("checkpoint capacity exhausted before work")]
    Capacity,
    #[error(
        "checkpoint lease is fenced; latest live observations must not be replaced by an older copy"
    )]
    Fenced,
    #[error("checkpoint revision conflict")]
    Conflict,
    #[error("checkpoint store lock was poisoned")]
    Poisoned,
    #[error("checkpoint job is already active")]
    Active,
    #[error("checkpoint not found")]
    NotFound,
    #[error("checkpoint version or state is invalid")]
    Invalid,
    #[error("checkpoint scope or fresh capability surface changed")]
    ScopeChanged,
    #[error("unresolved execution may have effects; dispatch and resume are refused")]
    UnknownWork,
    #[error("restored runtime cannot recover request-local binary assets or generated images")]
    AssetsUnavailable,
    #[error("session budget exhausted before work")]
    Budget,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Position {
    Ready,
    ModelPending,
    Tools,
    DispatchPending,
    ControlPending,
    GenerationFinished,
    Finalized,
}

/// Portable state with the mandatory attempt tracker. No provider continuation or binary assets.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub version: u32,
    pub revision: u64,
    pub position: Position,
    pub scope: String,
    pub surface: String,
    pub model: String,
    pub effort: String,
    pub context_revision: u64,
    pub record: JobRecord,
    pub history: History,
    pub limits: PromptLimits,
    pub state: SessionState,
    pub pending_execution: Option<u32>,
    pub finalized: bool,
}
impl Checkpoint {
    fn validate(&self) -> Result<usize, CheckpointError> {
        if self.version != CHECKPOINT_VERSION
            || self.record.job.is_empty()
            || self.scope.len() > 256
            || self.surface.len() > 256
            || self.model.len() > 256
            || !matches!(
                self.effort.as_str(),
                "providerDefault" | "low" | "medium" | "high"
            )
            || self.state.transitions.len() > 1280
            || self
                .state
                .current_model
                .as_ref()
                .is_some_and(|m| m.model != self.model || m.effort.to_string() != self.effort)
            || self
                .state
                .control_scope
                .as_ref()
                .is_some_and(|s| s.job.as_str() != self.record.job)
            || self.record.user.len() > 128 * 1024
            || self.record.groups.len() > 128
            || self.state.spent.asset_fetches > 4
            || self.state.spent.control_attempts > 4
            || !self
                .state
                .accounting
                .validate(&self.record.job, self.state.spent.model_calls)
            || self.record.executions.iter().any(|r| {
                r.job != self.record.job
                    || r.tool.len() > 256
                    || r.capability.len() > 256
                    || r.evidence.len() > 16
                    || r.evidence.iter().any(|e| e.len() > 256)
            })
            || self.record.executions.len() > MAX_EXECUTIONS
            || self.state.spent.model_calls > 128
            || self.state.spent.model_calls > self.limits.max_steps
            || self.state.spent.capability_invocations > self.limits.max_capability_calls
            || self.record.executions.iter().any(|r| {
                r.result
                    .as_ref()
                    .is_some_and(|e| e.text.len() > MAX_EXCERPT_BYTES)
            })
        {
            return Err(CheckpointError::Invalid);
        }
        let bytes = serde_json::to_vec(self)
            .map_err(|error| {
                tracing::error!(cause_type = "checkpoint-encoding", %error);
                CheckpointError::Invalid
            })?
            .len();
        if bytes > MAX_CHECKPOINT_BYTES {
            return Err(CheckpointError::Capacity);
        }
        Ok(bytes)
    }
    pub fn validate_resume(&self, scope: &str, surface: &str) -> Result<(), CheckpointError> {
        self.validate()?;
        if self.scope != scope || self.surface != surface {
            return Err(CheckpointError::ScopeChanged);
        }
        if self.pending_execution.is_some()
            || self.record.has_unknown_work()
            || matches!(
                self.position,
                Position::ModelPending | Position::ControlPending
            )
            || self.record.groups.iter().any(|g| !g.complete())
        {
            return Err(CheckpointError::UnknownWork);
        }
        if self.finalized
            || self.state.accounting.finalized
            || self.state.accounting.invalid
            || self.state.control_fenced
        {
            return Err(CheckpointError::Fenced);
        }
        if self.state.image_generation_attempted || self.state.spent.asset_fetches != 0 {
            return Err(CheckpointError::AssetsUnavailable);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SaveReceipt {
    pub revision: u64,
}

/// A supplied store must enforce exclusive live leases and CAS. A persistence error fences that
/// lease: callers retain live observations and never load an older snapshot as a retry strategy.
pub trait CheckpointStore: Send + Sync {
    fn load(&self, job: &str) -> Result<Checkpoint, CheckpointError>;
    fn acquire(&self, job: &str, new: bool) -> Result<String, CheckpointError>;
    fn compare_and_save(
        &self,
        lease: &str,
        expected: u64,
        checkpoint: &Checkpoint,
    ) -> Result<SaveReceipt, CheckpointError>;
    fn release(&self, job: &str, lease: &str, fenced: bool);
}
struct Entry {
    checkpoint: Option<Checkpoint>,
    lease: Option<String>,
    fenced: bool,
    touched: u64,
}
#[derive(Default)]
pub struct MemoryCheckpointStore {
    entries: Mutex<BTreeMap<String, Entry>>,
}
impl CheckpointStore for MemoryCheckpointStore {
    fn load(&self, job: &str) -> Result<Checkpoint, CheckpointError> {
        let entries = self.entries.lock().map_err(|error| {
            tracing::error!(cause_type = "checkpoint-lock", %error);
            CheckpointError::Poisoned
        })?;
        let entry = entries.get(job).ok_or(CheckpointError::NotFound)?;
        if entry.fenced {
            return Err(CheckpointError::Fenced);
        }
        entry.checkpoint.clone().ok_or(CheckpointError::NotFound)
    }
    fn acquire(&self, job: &str, new: bool) -> Result<String, CheckpointError> {
        let mut entries = self.entries.lock().map_err(|error| {
            tracing::error!(cause_type = "checkpoint-lock", %error);
            CheckpointError::Poisoned
        })?;
        let touched = entries
            .values()
            .map(|e| e.touched)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(CheckpointError::Capacity)?;
        if let Some(entry) = entries.get(job) {
            if entry.fenced {
                return Err(CheckpointError::Fenced);
            }
            if entry.lease.is_some() {
                return Err(CheckpointError::Active);
            }
            if new {
                return Err(CheckpointError::Conflict);
            }
        } else if !new {
            return Err(CheckpointError::NotFound);
        }
        // Reserve the entire per-job ceiling before work; active jobs cannot be evicted.
        loop {
            let used: usize = entries
                .values()
                .map(|e| {
                    if e.lease.is_some() {
                        MAX_CHECKPOINT_BYTES
                    } else {
                        e.checkpoint.as_ref().map_or(0, |c| {
                            serde_json::to_vec(c).expect("checkpoint serializes").len()
                        })
                    }
                })
                .sum();
            if entries.len() < MAX_JOBS && used + MAX_CHECKPOINT_BYTES <= MAX_STORE_BYTES {
                break;
            }
            let oldest = entries
                .iter()
                .filter(|(id, e)| id.as_str() != job && e.lease.is_none())
                .min_by_key(|(_, e)| e.touched)
                .map(|(id, _)| id.clone())
                .ok_or(CheckpointError::Capacity)?;
            entries.remove(&oldest);
        }
        let lease = opaque_id();
        let entry = entries.entry(job.to_owned()).or_insert(Entry {
            checkpoint: None,
            lease: None,
            fenced: false,
            touched,
        });
        entry.lease = Some(lease.clone());
        entry.touched = touched;
        Ok(lease)
    }
    fn compare_and_save(
        &self,
        lease: &str,
        expected: u64,
        checkpoint: &Checkpoint,
    ) -> Result<SaveReceipt, CheckpointError> {
        checkpoint.validate()?;
        let mut entries = self.entries.lock().map_err(|error| {
            tracing::error!(cause_type = "checkpoint-lock", %error);
            CheckpointError::Poisoned
        })?;
        let entry = entries
            .get_mut(&checkpoint.record.job)
            .ok_or(CheckpointError::NotFound)?;
        if entry.fenced || entry.lease.as_deref() != Some(lease) {
            return Err(CheckpointError::Fenced);
        }
        if entry.checkpoint.as_ref().map_or(0, |c| c.revision) != expected {
            return Err(CheckpointError::Conflict);
        }
        let revision = expected.checked_add(1).ok_or(CheckpointError::Capacity)?;
        let mut next = checkpoint.clone();
        next.revision = revision;
        entry.checkpoint = Some(next);
        Ok(SaveReceipt { revision })
    }
    fn release(&self, job: &str, lease: &str, fenced: bool) {
        match self.entries.lock() {
            Ok(mut entries) => {
                if let Some(entry) = entries.get_mut(job)
                    && entry.lease.as_deref() == Some(lease)
                {
                    entry.lease = None;
                    entry.fenced |= fenced;
                }
            }
            Err(error) => tracing::error!(cause_type = "checkpoint-release-lock", %error),
        }
    }
}
pub fn memory_checkpoints() -> Arc<dyn CheckpointStore> {
    static STORE: OnceLock<Arc<MemoryCheckpointStore>> = OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(MemoryCheckpointStore::default()))
        .clone()
}

/// The live ledger outlives persistence errors. No observation is undone by a failed save.
pub struct ExecutionJournal<'a> {
    pub(crate) activity: Option<crate::activity::ActivityEmitter>,
    pub(crate) accounting: crate::accounting::JobAccounting,
    cancellation: Option<&'a dyn crate::session::CancellationProbe>,
    inner: Mutex<Live>,
    store: Arc<dyn CheckpointStore>,
    lease: String,
}
struct Live {
    checkpoint: Checkpoint,
    error: Option<CheckpointError>,
}
impl<'a> ExecutionJournal<'a> {
    pub(crate) fn new(
        store: Arc<dyn CheckpointStore>,
        checkpoint: Checkpoint,
        new: bool,
        accounting: Option<&crate::accounting::JobAccounting>,
    ) -> Result<Self, CheckpointError> {
        let accounting = accounting.cloned().unwrap_or_default();
        let lease = store.acquire(&checkpoint.record.job, new)?;
        if let Err(error) = accounting.install(checkpoint.state.accounting.clone(), store.clone()) {
            store.release(&checkpoint.record.job, &lease, false);
            return Err(error);
        }
        let journal = Self {
            activity: None,
            accounting,
            cancellation: None,
            inner: Mutex::new(Live {
                checkpoint,
                error: None,
            }),
            store,
            lease,
        };
        journal.update(|_| {})?;
        Ok(journal)
    }
    pub(crate) fn with_activity(
        mut self,
        activity: Option<crate::activity::ActivityEmitter>,
    ) -> Self {
        self.activity = activity;
        self
    }
    pub(crate) fn with_cancellation(
        mut self,
        cancellation: Option<&'a dyn crate::session::CancellationProbe>,
    ) -> Self {
        self.cancellation = cancellation;
        self
    }
    pub(crate) fn cancelled(&self) -> bool {
        self.cancellation
            .is_some_and(crate::session::CancellationProbe::is_cancelled)
    }
    pub(crate) fn snapshot(&self) -> Checkpoint {
        let mut snapshot = self
            .inner
            .lock()
            .expect("live checkpoint")
            .checkpoint
            .clone();
        snapshot.state.accounting = self.accounting.snapshot();
        snapshot
    }
    pub(crate) fn error(&self) -> Option<CheckpointError> {
        self.inner.lock().expect("live checkpoint").error
    }
    pub(crate) fn update(&self, f: impl FnOnce(&mut Checkpoint)) -> Result<(), CheckpointError> {
        let mut live = self.inner.lock().map_err(|error| {
            tracing::error!(cause_type = "live-checkpoint-lock", %error);
            CheckpointError::Poisoned
        })?;
        f(&mut live.checkpoint); // preserve newly observed facts even when already fenced
        live.checkpoint.state.accounting = self.accounting.snapshot();
        // Independently bound model-facing groups without erasing the execution ledger. Keep a
        // labelled position marker for an omitted batch rather than orphaning its results.
        while serde_json::to_vec(&live.checkpoint.record.groups)
            .expect("groups serialize")
            .len()
            > crate::context::MAX_GROUP_BYTES
        {
            let Some(group) = live
                .checkpoint
                .record
                .groups
                .iter_mut()
                .find(|g| !g.omitted)
            else {
                break;
            };
            group.calls.clear();
            group.results.clear();
            group.omitted = true;
        }
        if let Some(error) = live.error {
            return Err(error);
        }
        match self
            .store
            .compare_and_save(&self.lease, live.checkpoint.revision, &live.checkpoint)
        {
            Ok(receipt) => {
                live.checkpoint.revision = receipt.revision;
                Ok(())
            }
            Err(error) => {
                live.error = Some(error);
                Err(error)
            }
        }
    }
    pub(crate) fn reserve(&self, capability: &str) -> Result<u32, CheckpointError> {
        // Model-selected escape-hatch names must not poison an otherwise valid checkpoint.
        capability
            .parse::<dekopon_core::CapabilityId>()
            .map_err(|error| {
                tracing::debug!(cause_type = "invalid-capability-identifier", reason = ?std::mem::discriminant(&error));
                CheckpointError::Invalid
            })?;
        if let Some(error) = self.error() {
            return Err(error);
        }
        let snapshot = self.snapshot();
        if snapshot.record.has_unknown_work() || snapshot.pending_execution.is_some() {
            return Err(CheckpointError::UnknownWork);
        }
        if snapshot.record.executions.len() >= MAX_EXECUTIONS
            || snapshot.state.spent.capability_invocations >= snapshot.limits.max_capability_calls
        {
            return Err(CheckpointError::Budget);
        }
        let sequence = snapshot.record.executions.len() as u32 + 1;
        let reserved = self.update(|c| {
            c.state.spent.capability_invocations += 1;
            c.position = Position::DispatchPending;
            c.pending_execution = Some(sequence);
            c.record.executions.push(ExecutionRecord {
                job: c.record.job.clone(),
                call: c
                    .state
                    .accounting
                    .calls
                    .iter()
                    .rev()
                    .find(|call| call.kind == crate::accounting::CallKind::Chat)
                    .map_or(c.state.spent.model_calls, |call| call.sequence),
                tool: c.state.current_tool.clone(),
                sequence,
                capability: capability.to_owned(),
                provenance: ExecutionProvenance::DirectReadOnly,
                invocation: None,
                evidence: Vec::new(),
                outcome: ExecutionOutcome::Unknown,
                result: None,
            });
        });
        if let Err(error) = reserved {
            // Persistence failed before dispatch; the live record can truthfully say not-executed.
            if let Err(persistence) =
                self.observe(sequence, |r| r.outcome = ExecutionOutcome::NotExecuted)
            {
                tracing::warn!(cause_type = "checkpoint-reservation-fenced", cause = %persistence);
            }
            return Err(error);
        }
        Ok(sequence)
    }
    pub(crate) fn observe(
        &self,
        sequence: u32,
        observation: impl FnOnce(&mut ExecutionRecord),
    ) -> Result<(), CheckpointError> {
        self.update(|c| {
            if let Some(record) = c
                .record
                .executions
                .iter_mut()
                .find(|r| r.sequence == sequence)
            {
                observation(record);
            }
            c.pending_execution = None;
            c.position = Position::Tools;
        })
    }
    pub(crate) fn failure(&self, error: CheckpointError) {
        self.inner
            .lock()
            .expect("live checkpoint")
            .error
            .get_or_insert(error);
    }
}
impl Drop for ExecutionJournal<'_> {
    fn drop(&mut self) {
        let live = self.inner.lock().unwrap_or_else(|error| {
            tracing::error!(cause_type = "checkpoint-drop-lock", %error);
            error.into_inner()
        });
        self.store.release(
            &live.checkpoint.record.job,
            &self.lease,
            live.error.is_some(),
        );
    }
}

/// Host delivery updates the latest snapshot once. It is not execution authority and never retries.
pub fn finalize_delivery(
    job: &str,
    delivery: DeliveryDisposition,
    accounting: &crate::accounting::JobAccounting,
) -> Result<(), CheckpointError> {
    if accounting.snapshot().job != job {
        return Err(CheckpointError::Invalid);
    }
    if accounting.finalize(&delivery) {
        Ok(())
    } else {
        Err(CheckpointError::Fenced)
    }
}

pub(crate) fn result_excerpt(text: &str) -> Excerpt {
    Excerpt::new(text, MAX_EXCERPT_BYTES)
}

#[cfg(test)]
mod tests;
