//! One factual ledger per logical job. Aggregation levels are projections, never additive spends.
use crate::{
    checkpoint::{CheckpointError, ExecutionJournal},
    control::{ModelIdentity, TransitionOutcome, TransitionRecord},
    history::DeliveryDisposition,
};
use dekopon_model::{
    model::ModelUsage,
    usage::{
        AccountingError, AttemptKind, AttemptRecorder, ObservationPrecedence, USAGE_FIELD_NAMES,
        UsageObservation, conflicting_fields,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

const MAX_CALLS: usize = 129; // 128 chat completions plus the one image-generation call
const MAX_ATTEMPTS: usize = 2; // Codex may retry one explicit 401, never uncertain inference
pub const ACCOUNTING_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TokenCount {
    /// Sum of reported fields. None means arithmetic overflow, not zero or a saturated count.
    pub known: Option<u64>,
    pub unreported: u32,
    pub invalid: bool,
}
impl Default for TokenCount {
    fn default() -> Self {
        Self {
            known: Some(0),
            unreported: 0,
            invalid: false,
        }
    }
}
impl TokenCount {
    fn add(&mut self, value: Option<u64>, invalid: bool) {
        self.invalid |= invalid;
        match value {
            Some(value) => {
                self.known = self.known.and_then(|sum| sum.checked_add(value));
                self.invalid |= self.known.is_none();
            }
            None => self.unreported += 1, // bounded by MAX_CALLS * MAX_ATTEMPTS
        }
    }
    pub fn complete(&self) -> Option<u64> {
        (self.unreported == 0 && !self.invalid)
            .then_some(self.known)
            .flatten()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TokenTotals {
    pub attempts: u32,
    pub unobserved_calls: u32,
    pub input: TokenCount,
    pub cached_input: TokenCount,
    pub output: TokenCount,
    pub reasoning_output: TokenCount,
    pub provider_total: TokenCount,
}
impl TokenTotals {
    pub(crate) fn add(&mut self, observation: Option<UsageObservation>) {
        self.attempts += 1;
        let observation = observation.unwrap_or_default();
        let f = observation.usage.fields();
        let mut invalid = observation.invalid;
        if let (Some(input), Some(cached)) = (f[0], f[1]) {
            invalid[1] |= cached > input;
        }
        if let (Some(output), Some(reasoning)) = (f[2], f[3]) {
            invalid[3] |= reasoning > output;
        }
        if let (Some(input), Some(output), Some(total)) = (f[0], f[2], f[4]) {
            invalid[4] |= input.checked_add(output) != Some(total);
        }
        for (i, count) in [
            &mut self.input,
            &mut self.cached_input,
            &mut self.output,
            &mut self.reasoning_output,
            &mut self.provider_total,
        ]
        .into_iter()
        .enumerate()
        {
            count.add(f[i], invalid[i]);
        }
    }
    fn unknown_call(&mut self) {
        self.add(None);
        self.attempts -= 1;
        self.unobserved_calls += 1;
    }
    /// Cached and reasoning counts are already included, never added again.
    pub fn input_plus_output(&self) -> Option<u64> {
        self.input.complete()?.checked_add(self.output.complete()?)
    }
    pub fn usage(&self) -> ModelUsage {
        ModelUsage::from_fields([
            self.input.complete(),
            self.cached_input.complete(),
            self.output.complete(),
            self.reasoning_output.complete(),
            self.provider_total.complete(),
        ])
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CallKind {
    Chat,
    Image,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CallOutcome {
    #[default]
    Pending,
    Succeeded,
    Failed,
    Cancelled,
    Abandoned,
}
impl CallOutcome {
    fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttemptRecord {
    pub sequence: u32,
    pub kind: AttemptKind,
    pub observation: Option<UsageObservation>,
    /// Which report `observation` came from, so a terminal one is not displaced by an interim one.
    #[serde(default)]
    pub precedence: ObservationPrecedence,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CallRecord {
    pub sequence: u32,
    pub model_turn: u32,
    pub segment: u32,
    pub identity: ModelIdentity,
    pub kind: CallKind,
    pub attempts: Vec<AttemptRecord>,
    /// No attempt recorded does not prove no transmission: an invalid adapter is unknown.
    pub attempts_complete: bool,
    pub outcome: CallOutcome,
    pub reason: String,
    pub duration_ms: u64,
    pub answer_present: bool,
    pub event_sequence: Option<u32>,
}
impl CallRecord {
    pub fn totals(&self) -> TokenTotals {
        let mut totals = TokenTotals::default();
        for attempt in &self.attempts {
            totals.add(attempt.observation);
        }
        if !self.attempts_complete && self.attempts.is_empty() {
            totals.unknown_call();
        }
        totals
    }
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelTotals {
    pub identity: ModelIdentity,
    pub totals: TokenTotals,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SegmentTotals {
    pub segment: u32,
    pub totals: TokenTotals,
}
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AccountingTotals {
    pub cumulative: TokenTotals,
    pub per_model: Vec<ModelTotals>,
    pub segments: Vec<SegmentTotals>,
}

/// Checkpointed source of truth. Totals are bounded checked projections, not independently mutable
/// counters. Restoring this value neither emits observations nor adds previously observed spend.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TokenTracker {
    pub job: String,
    pub segment: u32,
    pub event_sequence: u32,
    pub calls: Vec<CallRecord>,
    pub transition_sequence: u32,
    pub finalized: bool,
    pub generation: CallOutcome,
    pub generation_reason: String,
    pub delivery: String,
    pub invalid: bool,
    reported_calls: usize,
}
impl TokenTracker {
    pub fn totals(&self) -> AccountingTotals {
        let mut result = AccountingTotals::default();
        for call in &self.calls {
            let index = match result
                .per_model
                .iter()
                .position(|m| m.identity == call.identity)
            {
                Some(index) => index,
                None => {
                    result.per_model.push(ModelTotals {
                        identity: call.identity.clone(),
                        totals: TokenTotals::default(),
                    });
                    result.per_model.len() - 1
                }
            };
            let segment = match result
                .segments
                .iter()
                .position(|s| s.segment == call.segment)
            {
                Some(index) => index,
                None => {
                    result.segments.push(SegmentTotals {
                        segment: call.segment,
                        totals: TokenTotals::default(),
                    });
                    result.segments.len() - 1
                }
            };
            for attempt in &call.attempts {
                result.cumulative.add(attempt.observation);
                result.per_model[index].totals.add(attempt.observation);
                result.segments[segment].totals.add(attempt.observation);
            }
            if call.attempts.is_empty() && !call.attempts_complete {
                result.cumulative.unknown_call();
                result.per_model[index].totals.unknown_call();
                result.segments[segment].totals.unknown_call();
            }
        }
        result
    }
    pub(crate) fn validate(&self, job: &str, model_calls: u32) -> bool {
        self.job == job
            && self.calls.len() <= MAX_CALLS
            && self.segment <= 4
            && self.transition_sequence <= 1280
            && self.event_sequence <= 4096
            && self.reported_calls <= self.calls.len()
            && self
                .calls
                .iter()
                .filter(|c| c.kind == CallKind::Chat)
                .count()
                == model_calls as usize
            && self.calls.iter().enumerate().all(|(i, c)| {
                c.sequence as usize == i + 1
                    && c.segment <= self.segment
                    && c.attempts.len() <= MAX_ATTEMPTS
                    && c.identity.model.len() <= 256
                    && c.identity.backend.len() <= 64
                    && c.reason.len() <= 64
                    && c.attempts
                        .iter()
                        .enumerate()
                        .all(|(i, a)| a.sequence as usize == i + 1)
                    && c.event_sequence
                        .is_none_or(|seq| seq <= self.event_sequence)
            })
    }
    fn event(&mut self) -> u32 {
        self.event_sequence += 1;
        self.event_sequence
    }
}

/// Shared live ledger/finalizer. The final owner dropping after synchronous workers settle emits
/// an abandoned/unknown-delivery terminal record. Process death is not a completed-spend claim.
#[derive(Clone, Default)]
pub struct JobAccounting(Arc<Mutex<LiveAccounting>>);
#[derive(Default)]
struct LiveAccounting {
    tracker: TokenTracker,
    span: Option<tracing::Span>,
    store: Option<Arc<dyn crate::checkpoint::CheckpointStore>>,
}
impl JobAccounting {
    fn lock(&self) -> std::sync::MutexGuard<'_, LiveAccounting> {
        self.0.lock().unwrap_or_else(|e| {
            tracing::error!(cause_type = "accounting-lock", %e);
            let mut live = e.into_inner();
            live.tracker.invalid = true;
            live
        })
    }
    pub fn snapshot(&self) -> TokenTracker {
        self.lock().tracker.clone()
    }
    pub(crate) fn install(
        &self,
        tracker: TokenTracker,
        store: Arc<dyn crate::checkpoint::CheckpointStore>,
    ) -> Result<(), CheckpointError> {
        let mut live = self.lock();
        if !live.tracker.job.is_empty() {
            return if !live.tracker.finalized && live.tracker == tracker {
                Ok(())
            } else {
                Err(CheckpointError::Fenced)
            };
        }
        live.store = Some(store);
        live.span = Some(
            tracing::info_span!("accounting.model.job", accounting.version = ACCOUNTING_VERSION, job.id = %tracker.job),
        );
        live.tracker = tracker;
        Ok(())
    }
    pub(crate) fn span(&self) -> tracing::Span {
        self.lock().span.clone().unwrap_or_else(tracing::Span::none)
    }
    pub(crate) fn reserve(
        &self,
        identity: ModelIdentity,
        kind: CallKind,
        model_turn: u32,
    ) -> Result<u32, AccountingError> {
        let mut live = self.lock();
        let t = &mut live.tracker;
        if t.finalized || t.invalid || t.calls.len() >= MAX_CALLS {
            return Err(AccountingError("job fenced or full"));
        }
        let sequence = t.calls.len() as u32 + 1;
        t.calls.push(CallRecord {
            sequence,
            model_turn,
            segment: t.segment,
            identity,
            kind,
            attempts: vec![],
            attempts_complete: false,
            outcome: CallOutcome::Pending,
            reason: String::new(),
            duration_ms: 0,
            answer_present: false,
            event_sequence: None,
        });
        Ok(sequence)
    }
    pub(crate) fn generation(&self, outcome: CallOutcome, reason: &str) {
        let mut live = self.lock();
        live.tracker.generation = outcome;
        live.tracker.generation_reason = reason.to_owned();
    }
    /// Consume once, independently of storage/export success. Accepted text never enters telemetry.
    pub fn finalize(&self, delivery: &DeliveryDisposition) -> bool {
        let mut live = self.lock();
        finalize(&mut live, delivery)
    }
    /// Informational projection only. A restored cursor cannot report old observations twice.
    /// The legacy protocol's `modelCalls`/`unreportedCalls` now count attempt observations.
    ///
    /// One bad field no longer blanks the delta, and the cursor moves only when a report is
    /// emitted. `provider_total != input + output` is a real disagreement about *that* field —
    /// several OpenAI-compatible endpoints define the total differently — and treating it as
    /// grounds to discard the input and output counts left the broker's live token view empty
    /// forever, since the cursor had already advanced past them.
    pub fn take_report(&self) -> Option<dekopon_broker_protocol::ModelUsageReport> {
        let mut live = self.lock();
        let job = live.tracker.job.clone();
        let tracker = &mut live.tracker;
        let start = tracker.reported_calls;
        let end = tracker
            .calls
            .iter()
            .take_while(|c| c.event_sequence.is_some())
            .count();
        let mut totals = TokenTotals::default();
        for call in &tracker.calls[start..end] {
            for attempt in &call.attempts {
                totals.add(attempt.observation);
            }
            if call.attempts.is_empty() && !call.attempts_complete {
                totals.unknown_call();
            }
        }
        if totals.attempts + totals.unobserved_calls == 0 {
            // Nothing observed, so nothing to report — and nothing to advance past either. Moving
            // the cursor here would skip these calls for good if an observation lands later.
            return None;
        }
        let calls = u64::from(totals.attempts + totals.unobserved_calls);
        // Per field, not per report: a field this process cannot trust is unreported for every call
        // in the delta, and the other four still carry what the provider actually said.
        let field = |count: &TokenCount, name: &str| match (count.invalid, count.known) {
            (false, Some(known)) => (known, u64::from(count.unreported)),
            _ => {
                tracing::warn!(
                    cause_type = "accounting-field-unreported",
                    job.id = %job,
                    usage.field = name,
                    usage.calls = calls,
                    "usage field is unknown for these calls and is reported as unreported"
                );
                (0, calls)
            }
        };
        let input = field(&totals.input, USAGE_FIELD_NAMES[0]);
        let cached_input = field(&totals.cached_input, USAGE_FIELD_NAMES[1]);
        let output = field(&totals.output, USAGE_FIELD_NAMES[2]);
        let reasoning_output = field(&totals.reasoning_output, USAGE_FIELD_NAMES[3]);
        let provider_total = field(&totals.provider_total, USAGE_FIELD_NAMES[4]);
        tracker.reported_calls = end;
        Some(dekopon_broker_protocol::ModelUsageReport {
            model_calls: calls,
            input_tokens: input.0,
            input_unreported_calls: input.1,
            cached_input_tokens: cached_input.0,
            cached_input_unreported_calls: cached_input.1,
            output_tokens: output.0,
            output_unreported_calls: output.1,
            reasoning_output_tokens: reasoning_output.0,
            reasoning_unreported_calls: reasoning_output.1,
            total_tokens: provider_total.0,
            total_unreported_calls: provider_total.1,
        })
    }
    pub(crate) fn transition(
        &self,
        record: &TransitionRecord,
        before: AccountingTotals,
        elapsed: std::time::Duration,
    ) {
        let mut live = self.lock();
        let parent = live.span.clone();
        let t = &mut live.tracker;
        if record.sequence <= t.transition_sequence {
            return;
        }
        let event = t.event();
        t.transition_sequence = record.sequence;
        // Restore admission changes clients, not historical segment membership.
        if record.outcome == TransitionOutcome::Applied && record.requesting_call.is_some() {
            t.segment += 1;
        }
        let accounting = TransitionAccounting {
            version: ACCOUNTING_VERSION,
            job: t.job.clone(),
            event_sequence: event,
            record: record.clone(),
            before,
            after_segment: t.segment,
            duration_ms: millis(elapsed),
        };
        let span = tracing::info_span!(parent: parent.as_ref().and_then(tracing::Span::id), "accounting.model.transition", job.id=%t.job, transition.sequence=record.sequence, event.sequence=event);
        span.in_scope(|| tracing::info!(target:"dekopon_harness::audit", { audit.event="accounting.model.transition", accounting.version=ACCOUNTING_VERSION, job.id=%t.job, transition.sequence=record.sequence, event.sequence=event, accounting=%json(&accounting) }, "model transition accounted"));
    }
}
/// The one `accounting.model.call` span shape, for every path that opens one.
///
/// Two callsites used to build this span with two different field sets, so a call the finalize
/// sweep closed as abandoned exported without the model identity, the segment, or the usage fields
/// an ordinary call carries — and a query filtering on any of them silently omitted exactly the
/// calls whose outcome was in doubt.
fn call_span(tracker: &TokenTracker, parent: Option<tracing::Id>, call: u32) -> tracing::Span {
    let record = &tracker.calls[call as usize - 1];
    let identity = &record.identity;
    tracing::info_span!(parent: parent, "accounting.model.call", accounting.version=ACCOUNTING_VERSION, job.id=%tracker.job, call.sequence=call, model.turn=record.model_turn, segment.sequence=tracker.segment, model.configured=identity.configured.as_ref().map(|m|m.as_str()), model.backend=%identity.backend, model.name=%identity.model, model.effort=%identity.effort, usage.input_tokens=tracing::field::Empty, usage.cached_input_tokens=tracing::field::Empty, usage.output_tokens=tracing::field::Empty, usage.reasoning_output_tokens=tracing::field::Empty, usage.total_tokens=tracing::field::Empty, outcome=tracing::field::Empty, reason=tracing::field::Empty, duration_ms=tracing::field::Empty, event.sequence=tracing::field::Empty)
}
fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
fn json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("typed accounting serializes")
}
fn delivery_name(delivery: &DeliveryDisposition) -> &'static str {
    match delivery {
        DeliveryDisposition::Pending | DeliveryDisposition::Unknown => "unknown",
        DeliveryDisposition::Accepted { .. } => "accepted",
        DeliveryDisposition::Suppressed => "suppressed",
        DeliveryDisposition::Cancelled => "cancelled",
        DeliveryDisposition::Failed => "failed",
        DeliveryDisposition::Partial => "partial",
    }
}
#[derive(Serialize)]
struct TransitionAccounting {
    version: u32,
    job: String,
    event_sequence: u32,
    record: TransitionRecord,
    before: AccountingTotals,
    after_segment: u32,
    duration_ms: u64,
}
#[derive(Serialize)]
struct JobRecord<'a> {
    version: u32,
    job: &'a str,
    event_sequence: u32,
    generation: CallOutcome,
    generation_reason: &'a str,
    delivery: &'a str,
    invalid: bool,
    totals: AccountingTotals,
    input_plus_output: Option<u64>,
}
fn finalize(live: &mut LiveAccounting, disposition: &DeliveryDisposition) -> bool {
    let delivery = delivery_name(disposition);
    if live.tracker.job.is_empty() || live.tracker.finalized {
        return false;
    }
    // An unwound call can have observations even though its result was never received.
    for index in 0..live.tracker.calls.len() {
        if live.tracker.calls[index].event_sequence.is_none() {
            let span = call_span(
                &live.tracker,
                live.span.as_ref().and_then(tracing::Span::id),
                index as u32 + 1,
            );
            span.in_scope(|| {
                finish_call(
                    live,
                    index as u32 + 1,
                    CallOutcome::Abandoned,
                    "abandoned",
                    0,
                    false,
                )
            });
        }
    }
    let parent = live.span.as_ref().and_then(tracing::Span::id);
    let t = &mut live.tracker;
    if t.generation == CallOutcome::Pending {
        t.generation = CallOutcome::Abandoned;
        t.generation_reason = "abandoned".into();
    }
    t.finalized = true;
    t.delivery = delivery.to_owned();
    let event = t.event();
    let record = JobRecord {
        version: ACCOUNTING_VERSION,
        job: &t.job,
        event_sequence: event,
        generation: t.generation,
        generation_reason: &t.generation_reason,
        delivery,
        invalid: t.invalid,
        input_plus_output: t.totals().cumulative.input_plus_output(),
        totals: t.totals(),
    };
    tracing::info!(target:"dekopon_harness::audit", parent:parent, { audit.event="accounting.model.job", accounting.version=ACCOUNTING_VERSION, job.id=%t.job, event.sequence=event, outcome=t.generation.name(), delivery, accounting=%json(&record) }, "job accounted");
    if let Some(store) = &live.store
        && let Err(error) = persist_terminal(store.as_ref(), t, disposition)
    {
        tracing::error!(cause_type="accounting-terminal-checkpoint", %error);
    }
    true
}
fn persist_terminal(
    store: &dyn crate::checkpoint::CheckpointStore,
    tracker: &TokenTracker,
    delivery: &DeliveryDisposition,
) -> Result<(), CheckpointError> {
    let lease = store.acquire(&tracker.job, false)?;
    let result = (|| {
        let mut c = store.load(&tracker.job)?;
        c.state.accounting = tracker.clone();
        c.finalized = true;
        c.position = crate::checkpoint::Position::Finalized;
        c.record.delivery = delivery.clone();
        let measured = c.measure()?;
        store.compare_and_save(&lease, c.revision, &c, measured)
    })();
    store.release(&tracker.job, &lease, result.is_err());
    result.map(|_| ())
}
impl Drop for LiveAccounting {
    fn drop(&mut self) {
        finalize(self, &DeliveryDisposition::Unknown);
    }
}

pub(crate) struct CallRecorder<'a, 'b> {
    journal: &'a ExecutionJournal<'b>,
    call: u32,
    started: Instant,
    span: tracing::Span,
}
impl<'a, 'b> CallRecorder<'a, 'b> {
    pub(crate) fn new(
        journal: &'a ExecutionJournal<'b>,
        identity: ModelIdentity,
        kind: CallKind,
        model_turn: u32,
    ) -> Result<Self, crate::session::PromptError> {
        let call = journal
            .accounting
            .reserve(identity.clone(), kind, model_turn)?;
        journal.update(|_| {})?;
        Ok(Self::reserved(journal, call))
    }
    pub(crate) fn reserved(journal: &'a ExecutionJournal<'b>, call: u32) -> Self {
        let tracker = journal.accounting.snapshot();
        let span = call_span(&tracker, journal.accounting.span().id(), call);
        Self {
            journal,
            call,
            started: Instant::now(),
            span,
        }
    }
    pub(crate) fn span(&self) -> tracing::Span {
        self.span.clone()
    }
    pub(crate) fn finish(
        &self,
        outcome: CallOutcome,
        reason: &str,
        answer: bool,
    ) -> Result<(), CheckpointError> {
        self.span.record("outcome", outcome.name());
        self.span.record("reason", reason);
        self.span
            .record("duration_ms", millis(self.started.elapsed()));
        {
            let mut live = self.journal.accounting.lock();
            let usage = live.tracker.calls[self.call as usize - 1].totals().usage();
            for (name, value) in [
                "usage.input_tokens",
                "usage.cached_input_tokens",
                "usage.output_tokens",
                "usage.reasoning_output_tokens",
                "usage.total_tokens",
            ]
            .into_iter()
            .zip(usage.fields())
            {
                if let Some(value) = value {
                    self.span.record(name, value);
                }
            }
            self.span.in_scope(|| {
                finish_call(
                    &mut live,
                    self.call,
                    outcome,
                    reason,
                    millis(self.started.elapsed()),
                    answer,
                )
            });
        }
        self.span.record(
            "event.sequence",
            self.journal.accounting.snapshot().calls[self.call as usize - 1].event_sequence,
        );
        self.journal.update(|_| {})
    }
}
impl Drop for CallRecorder<'_, '_> {
    fn drop(&mut self) {
        if self.journal.accounting.snapshot().calls[self.call as usize - 1]
            .event_sequence
            .is_none()
            && let Err(error) = self.finish(CallOutcome::Abandoned, "abandoned", false)
        {
            tracing::error!(cause_type="abandoned-call-checkpoint", %error);
        }
    }
}
fn finish_call(
    live: &mut LiveAccounting,
    call: u32,
    outcome: CallOutcome,
    reason: &str,
    duration_ms: u64,
    answer: bool,
) {
    let t = &mut live.tracker;
    let index = call as usize - 1;
    if t.calls[index].event_sequence.is_some() {
        return;
    }
    let event = t.event();
    let c = &mut t.calls[index];
    c.outcome = outcome;
    c.reason = reason.to_owned();
    c.duration_ms = duration_ms;
    c.answer_present = answer;
    c.event_sequence = Some(event);
    let usage = c.totals().usage();
    tracing::info!(target:"dekopon_harness::audit", { audit.event="accounting.model.call", accounting.version=ACCOUNTING_VERSION, job.id=%t.job, call.sequence=call, event.sequence=event, segment.sequence=c.segment, model.turn=c.model_turn, model.kind=match c.kind { CallKind::Chat => "chat", CallKind::Image => "image" }, model.configured=c.identity.configured.as_ref().map(|m|m.as_str()), model.backend=%c.identity.backend, model.name=%c.identity.model, model.effort=%c.identity.effort, duration_ms, outcome=outcome.name(), reason, answer.present=answer, usage.input_tokens=usage.input_tokens, usage.cached_input_tokens=usage.cached_input_tokens, usage.output_tokens=usage.output_tokens, usage.reasoning_output_tokens=usage.reasoning_output_tokens, usage.total_tokens=usage.total_tokens, accounting=%json(c) }, "model call accounted");
}
impl AttemptRecorder for CallRecorder<'_, '_> {
    fn begin(&self, kind: AttemptKind) -> Result<u32, AccountingError> {
        let attempt = {
            let mut live = self.journal.accounting.lock();
            let t = &mut live.tracker;
            if t.finalized || t.invalid {
                return Err(AccountingError("job fenced"));
            }
            let c = &mut t.calls[self.call as usize - 1];
            if c.attempts.len() >= MAX_ATTEMPTS || c.outcome != CallOutcome::Pending {
                return Err(AccountingError("attempt limit or closed call"));
            }
            let sequence = c.attempts.len() as u32 + 1;
            c.attempts.push(AttemptRecord {
                sequence,
                kind,
                observation: None,
                precedence: ObservationPrecedence::Interim,
            });
            c.attempts_complete = true;
            sequence
        };
        self.journal.update(|_| {}).map_err(|e| {
            tracing::error!(cause_type="accounting-reservation", %e);
            AccountingError("checkpoint reservation")
        })?;
        Ok(attempt)
    }
    fn observe(&self, attempt: u32, observation: UsageObservation) -> Result<(), AccountingError> {
        self.observe_ranked(attempt, observation, ObservationPrecedence::Final)
    }
    /// Records one usage report, reconciling a disagreeing one instead of fencing the job.
    ///
    /// A second, differing report of the same attempt is provider-controlled data — duplicate
    /// `"usage"` keys in one object are legal JSON, and a stream can contradict itself — so making
    /// it fence the ledger handed a provider a way to end every later turn of the job and to make
    /// the checkpoint unresumable. The disagreeing *fields* become unknown, named in a warning; the
    /// job keeps counting. A terminal report still supersedes an interim one outright.
    fn observe_ranked(
        &self,
        attempt: u32,
        observation: UsageObservation,
        precedence: ObservationPrecedence,
    ) -> Result<(), AccountingError> {
        let index = attempt
            .checked_sub(1)
            .ok_or(AccountingError("attempt id"))? as usize;
        {
            let mut live = self.journal.accounting.lock();
            let job = live.tracker.job.clone();
            let (existing, existing_precedence, closed) = {
                let call = &live.tracker.calls[self.call as usize - 1];
                let recorded = call
                    .attempts
                    .get(index)
                    .ok_or(AccountingError("attempt id"))?;
                (
                    recorded.observation,
                    recorded.precedence,
                    call.event_sequence.is_some(),
                )
            };
            let mut install = |observation, precedence| {
                let recorded = &mut live.tracker.calls[self.call as usize - 1].attempts[index];
                recorded.observation = Some(observation);
                recorded.precedence = precedence;
            };
            match existing {
                // A first observation arriving after the call closed is this process losing track
                // of its own ledger, not a provider disagreeing with itself. That still fences.
                None if closed => {
                    live.tracker.invalid = true;
                    return Err(AccountingError("observation after closed call"));
                }
                None => install(observation, precedence),
                Some(existing) if existing != observation => {
                    if precedence > existing_precedence {
                        install(observation, precedence);
                    } else if precedence == existing_precedence {
                        let (merged, conflicts) = existing.reconcile(observation);
                        install(merged, existing_precedence);
                        tracing::warn!(
                            cause_type = "conflicting-usage-observation",
                            job.id = %job,
                            call.sequence = self.call,
                            attempt,
                            usage.fields = %conflicting_fields(conflicts),
                            "attempt reported disagreeing usage; those fields are unknown"
                        );
                    }
                    // An interim report never displaces the terminal one.
                }
                Some(_) => {}
            }
        }
        // Live evidence is installed before persistence can fail. Never retry from an older copy.
        self.journal.update(|_| {}).map_err(|e| {
            tracing::error!(cause_type="accounting-observation", %e);
            AccountingError("checkpoint observation")
        })
    }
}

#[cfg(test)]
pub(crate) fn fixture_tracker(job: &str, reports: &[[Option<u64>; 5]]) -> TokenTracker {
    TokenTracker {
        job: job.into(),
        event_sequence: reports.len() as u32,
        calls: reports
            .iter()
            .enumerate()
            .map(|(i, f)| CallRecord {
                sequence: i as u32 + 1,
                model_turn: i as u32 + 1,
                segment: 0,
                identity: ModelIdentity {
                    configured: None,
                    backend: "adapter".into(),
                    model: "fixture".into(),
                    effort: dekopon_core::Effort::ProviderDefault,
                },
                kind: CallKind::Chat,
                attempts: vec![AttemptRecord {
                    sequence: 1,
                    kind: AttemptKind::Adapter,
                    precedence: ObservationPrecedence::Final,
                    observation: Some(UsageObservation {
                        usage: ModelUsage::from_fields(*f),
                        invalid: [false; 5],
                    }),
                }],
                attempts_complete: true,
                outcome: CallOutcome::Succeeded,
                reason: "completed".into(),
                duration_ms: 0,
                answer_present: false,
                event_sequence: Some(i as u32 + 1),
            })
            .collect(),
        ..TokenTracker::default()
    }
}

#[cfg(test)]
mod tests;
