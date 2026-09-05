//! The live per-turn ledger one session writes its bounded state through. Nothing here persists.
use crate::{
    history::{
        DeliveryDisposition, Excerpt, ExecutionOutcome, ExecutionProvenance, ExecutionRecord,
        History, JobRecord, MAX_EXCERPT_BYTES, MAX_EXECUTIONS,
    },
    session::{PromptLimits, SessionState},
};
use serde::Serialize;
use std::{
    hash::{BuildHasher as _, Hasher as _},
    sync::Mutex,
};
use thiserror::Error;

/// Measures a value's JSON encoding without building it.
///
/// A bound is measured, never materialized: the group ceiling runs several times per tool call,
/// and `serde_json::to_vec` would allocate and discard a copy of the batches each time. The count
/// is the same one the encoder would have written, so there is still exactly one definition of
/// "how big is this".
fn encoded_len(value: &impl Serialize) -> Result<usize, JournalError> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_err(|error| {
        tracing::error!(cause_type = "journal-encoding", %error);
        JournalError::Invalid
    })?;
    #[cfg(test)]
    ENCODED_BYTES.with(|total| total.set(total.get() + counter.0));
    Ok(counter.0)
}

#[cfg(test)]
thread_local! {
    /// How many JSON bytes this thread has measured, so a test can pin the work per mutation.
    ///
    /// Bytes rather than calls, because "the groups and nothing else" is a claim about traversal
    /// cost: a mutation that measures only the model-facing batches has walked a fraction of the
    /// state, and one that measures the whole document has not. A call count cannot tell those
    /// apart. Per thread rather than per process: the total is only meaningful for one sequence of
    /// calls, and a process-wide one would make every test here observe its siblings' work.
    pub(crate) static ENCODED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn opaque_id() -> String {
    let mut a = std::collections::hash_map::RandomState::new().build_hasher();
    let mut b = std::collections::hash_map::RandomState::new().build_hasher();
    a.write_u8(1);
    b.write_u8(2);
    format!("job-{:016x}{:016x}", a.finish(), b.finish())
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum JournalError {
    #[error("bounded session state cannot grow further")]
    Capacity,
    #[error("job is fenced; latest live observations must not be replaced or continued")]
    Fenced,
    #[error("live state lock was poisoned")]
    Poisoned,
    #[error("session state is invalid")]
    Invalid,
    #[error("scope or fresh capability surface changed")]
    ScopeChanged,
    #[error("unresolved execution may have effects; dispatch is refused")]
    UnknownWork,
    #[error("session budget exhausted before work")]
    Budget,
}

/// One job's live state: the mandatory attempt tracker plus everything the turn has observed.
///
/// In-process and request-scoped, held by exactly one [`ExecutionJournal`] for the life of one
/// turn. Nothing persists it, resumes it or shares it between processes; no provider continuation,
/// binary asset, credential or grant is held here. A session hands its final copy to the caller
/// through [`FinalState`], and a fenced one through
/// [`crate::session::PromptError::Interrupted`].
#[derive(Clone, Debug, PartialEq)]
pub struct JobState {
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
}
impl JobState {
    /// Everything the state must satisfy after any mutation. Each field is bounded on its own.
    fn validate(&self) -> Result<(), JournalError> {
        if self.record.job.is_empty()
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
            return Err(JournalError::Invalid);
        }
        Ok(())
    }
}

/// Where a finished session leaves the state it ended with.
///
/// The engine records the job's turn into the caller's [`History`] as well, but that copy is
/// trimmed to the conversation's retention window on the way in. A host that must remember the
/// whole job — an unresolved execution whose model-facing text no longer fits the window, say —
/// reads the untrimmed record here instead. A session that never reached inference publishes
/// nothing, and a fenced one publishes the same state
/// [`crate::session::PromptError::Interrupted`] carries.
#[derive(Default)]
pub struct FinalState(Mutex<Option<JobState>>);
impl FinalState {
    pub(crate) fn publish(&self, state: JobState) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(state);
        }
    }
    /// Consumes the published state, leaving nothing behind for a second reader.
    #[must_use]
    pub fn take(&self) -> Option<JobState> {
        self.0.lock().map_or(None, |mut slot| slot.take())
    }
}

/// The live ledger. No observation is undone by a bookkeeping failure.
pub struct ExecutionJournal<'a> {
    pub(crate) activity: Option<crate::activity::ActivityEmitter>,
    pub(crate) accounting: crate::accounting::JobAccounting,
    cancellation: Option<&'a dyn crate::session::CancellationProbe>,
    inner: Mutex<Live>,
}
struct Live {
    job: JobState,
    error: Option<JournalError>,
}
impl<'a> ExecutionJournal<'a> {
    pub(crate) fn new(
        job: JobState,
        accounting: Option<&crate::accounting::JobAccounting>,
    ) -> Result<Self, JournalError> {
        let accounting = accounting.cloned().unwrap_or_default();
        accounting.install(job.state.accounting.clone())?;
        let journal = Self {
            activity: None,
            accounting,
            cancellation: None,
            inner: Mutex::new(Live { job, error: None }),
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
    /// Reads the live state, recovering a poisoned lock the way a read after a panic must.
    ///
    /// A panic inside an update closure must not turn every later read of the ledger into a second
    /// panic: the observations already recorded are exactly what a failing session still has to
    /// report. The write path (`update`) still refuses under a poisoned lock and fences the job.
    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.inner.lock().unwrap_or_else(|error| {
            tracing::error!(cause_type = "live-job-state-lock", %error);
            error.into_inner()
        })
    }
    pub(crate) fn snapshot(&self) -> JobState {
        let mut snapshot = self.live().job.clone();
        snapshot.state.accounting = self.accounting.snapshot();
        snapshot
    }
    pub(crate) fn error(&self) -> Option<JournalError> {
        self.live().error
    }
    pub(crate) fn update(&self, f: impl FnOnce(&mut JobState)) -> Result<(), JournalError> {
        let mut live = self.inner.lock().map_err(|error| {
            tracing::error!(cause_type = "live-job-state-lock", %error);
            JournalError::Poisoned
        })?;
        f(&mut live.job); // preserve newly observed facts even when already fenced
        live.job.state.accounting = self.accounting.snapshot();
        // Independently bound model-facing groups without erasing the execution ledger. Keep a
        // labelled position marker for an omitted batch rather than orphaning its results. Each
        // omission adjusts the running group total by its own before/after size, so trimming
        // re-encodes one group at a time rather than the whole list once per omission.
        //
        // Only the groups are measured. `update` runs several times per tool call and holds the
        // live lock while it does, so the one bound that needs an encoding pays for that field and
        // nothing else; every other field below is bounded by a length or a counter.
        let mut groups = encoded_len(&live.job.record.groups)?;
        let mut index = 0;
        while groups > crate::context::MAX_GROUP_BYTES {
            let Some(position) = live.job.record.groups[index..]
                .iter()
                .position(|g| !g.omitted)
                .map(|offset| index + offset)
            else {
                break;
            };
            let group = &mut live.job.record.groups[position];
            let before = encoded_len(group)?;
            group.calls.clear();
            group.results.clear();
            group.omitted = true;
            groups = groups - before + encoded_len(&live.job.record.groups[position])?;
            index = position + 1;
        }
        if let Some(error) = live.error {
            return Err(error);
        }
        if let Err(error) = live.job.validate() {
            live.error = Some(error);
            return Err(error);
        }
        Ok(())
    }
    pub(crate) fn reserve(&self, capability: &str) -> Result<u32, JournalError> {
        // Model-selected escape-hatch names must not poison an otherwise valid job.
        capability
            .parse::<dekopon_core::CapabilityId>()
            .map_err(|error| {
                tracing::debug!(cause_type = "invalid-capability-identifier", reason = ?std::mem::discriminant(&error));
                JournalError::Invalid
            })?;
        if let Some(error) = self.error() {
            return Err(error);
        }
        let snapshot = self.snapshot();
        if snapshot.record.has_unknown_work() || snapshot.pending_execution.is_some() {
            return Err(JournalError::UnknownWork);
        }
        if snapshot.record.executions.len() >= MAX_EXECUTIONS
            || snapshot.state.spent.capability_invocations >= snapshot.limits.max_capability_calls
        {
            return Err(JournalError::Budget);
        }
        let sequence = snapshot.record.executions.len() as u32 + 1;
        let reserved = self.update(|c| {
            c.state.spent.capability_invocations += 1;
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
            // The reservation was refused before dispatch; the live record can truthfully say
            // not-executed.
            if let Err(fenced) =
                self.observe(sequence, |r| r.outcome = ExecutionOutcome::NotExecuted)
            {
                tracing::warn!(cause_type = "journal-reservation-fenced", cause = %fenced);
            }
            return Err(error);
        }
        Ok(sequence)
    }
    pub(crate) fn observe(
        &self,
        sequence: u32,
        observation: impl FnOnce(&mut ExecutionRecord),
    ) -> Result<(), JournalError> {
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
        })
    }
    pub(crate) fn failure(&self, error: JournalError) {
        self.live().error.get_or_insert(error);
    }
}

/// Host delivery closes the job's accounting once. It is not execution authority and never retries.
pub fn finalize_delivery(
    job: &str,
    delivery: DeliveryDisposition,
    accounting: &crate::accounting::JobAccounting,
) -> Result<(), JournalError> {
    if accounting.snapshot().job != job {
        return Err(JournalError::Invalid);
    }
    if accounting.finalize(&delivery) {
        Ok(())
    } else {
        Err(JournalError::Fenced)
    }
}

pub(crate) fn result_excerpt(text: &str) -> Excerpt {
    Excerpt::new(text, MAX_EXCERPT_BYTES)
}

#[cfg(test)]
mod tests;
