use super::*;
use crate::{
    bootstrap::{BootstrapError, CapabilitySnapshot, SessionBootstrap},
    checkpoint::{Checkpoint, CheckpointStore, MemoryCheckpointStore, SaveReceipt},
    history::History,
    runtime::ScriptRuntime,
    session::{CancellationProbe, PromptError, PromptLimits, SessionEngine},
};
use dekopon_model::model::{
    AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool, ModelToolCall,
};
use std::sync::atomic::{AtomicBool, Ordering};

struct Runtime;
impl ScriptRuntime for Runtime {
    fn capability_snapshot(&self) -> Result<CapabilitySnapshot, BootstrapError> {
        Ok(CapabilitySnapshot::empty())
    }
    fn run_script(&self, _: &str, _: u32) -> dekopon_shell::ScriptOutcome {
        unreachable!("no script dispatch")
    }
}
struct Model {
    mode: &'static str,
    cancelled: AtomicBool,
}
impl CancellationProbe for Model {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}
fn observation() -> UsageObservation {
    UsageObservation {
        usage: ModelUsage::from_fields([Some(100), Some(60), Some(20), Some(10), Some(120)]),
        invalid: [false; 5],
    }
}
impl ChatModel for Model {
    fn complete(
        &self,
        _: &[ModelMessage],
        _: &[ModelTool],
        recorder: &dyn AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        let attempt = recorder.begin(AttemptKind::Adapter)?;
        if self.mode != "missing" {
            recorder.observe(attempt, observation())?;
            recorder.observe(attempt, observation())?; // identical delivery is idempotent
        }
        if self.mode == "conflict" {
            let mut other = observation();
            other.usage.input_tokens = Some(101);
            recorder.observe(attempt, other)?;
        }
        if self.mode == "model-error" {
            return Err(ModelError::Response("late decode failure".into()));
        }
        if self.mode == "cancelled" {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        Ok(AssistantTurn {
            content: Some("answer".into()),
            usage: None,
            replay_items: vec![],
            tool_calls: if self.mode == "tool-error" {
                vec![ModelToolCall {
                    id: String::new(),
                    kind: "function".into(),
                    function: ModelFunctionCall {
                        name: "invalid-tool".into(),
                        arguments: "{}".into(),
                    },
                }]
            } else {
                vec![]
            },
        })
    }
}
fn limits() -> PromptLimits {
    PromptLimits {
        max_steps: 2,
        max_capability_calls: 2,
    }
}

#[test]
fn observed_usage_survives_later_model_tool_cancellation_and_delivery_failures_once() {
    for mode in [
        "success",
        "model-error",
        "tool-error",
        "cancelled",
        "missing",
        "conflict",
    ] {
        let model = Model {
            mode,
            cancelled: AtomicBool::new(false),
        };
        let ledger = JobAccounting::default();
        let store = Arc::new(MemoryCheckpointStore::default());
        let outcome = SessionEngine::new(&model, &Runtime)
            .with_checkpoint_store(store.clone())
            .run(
                SessionBootstrap::new("prompt", limits(), "fixture")
                    .with_accounting(&ledger)
                    .with_cancellation(&model),
                &mut History::default(),
            );
        assert_eq!(
            outcome.is_ok(),
            matches!(mode, "success" | "missing"),
            "{mode}"
        );
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.calls.len(), 1);
        let total = snapshot.totals().cumulative;
        assert_eq!(total.attempts, 1);
        if mode == "missing" {
            assert_eq!(total.input.complete(), None);
            assert_eq!(total.input.unreported, 1);
        } else {
            assert_eq!(total.input.known, Some(100));
            assert_eq!(total.cached_input.known, Some(60));
            assert_eq!(total.output.known, Some(20));
            assert_eq!(total.reasoning_output.known, Some(10));
            assert_eq!(total.input_plus_output(), Some(120));
        }
        assert_eq!(snapshot.invalid, mode == "conflict");
        assert_eq!(
            snapshot.calls[0].outcome,
            if mode == "cancelled" {
                CallOutcome::Cancelled
            } else if matches!(mode, "model-error" | "conflict") {
                CallOutcome::Failed
            } else {
                CallOutcome::Succeeded
            }
        );
        assert!(ledger.finalize(&DeliveryDisposition::Failed));
        assert!(!ledger.finalize(&DeliveryDisposition::Accepted {
            text: "must not replace failure".into()
        }));
        let saved = store.load(&snapshot.job).unwrap();
        assert!(saved.finalized && saved.state.accounting.finalized);
        assert_eq!(saved.state.accounting.totals(), ledger.snapshot().totals());
        assert_eq!(
            saved.validate_resume("direct", &saved.surface),
            Err(CheckpointError::Fenced)
        );
    }
}

#[test]
fn subsets_missing_fields_invalidity_and_overflow_never_become_extra_or_free_tokens() {
    let mut total = TokenTotals::default();
    total.add(Some(observation()));
    assert_eq!(total.input_plus_output(), Some(120));
    let mut invalid = observation();
    invalid.usage.cached_input_tokens = Some(101);
    invalid.usage.reasoning_output_tokens = Some(21);
    invalid.usage.total_tokens = Some(999);
    total.add(Some(invalid));
    assert_eq!(total.cached_input.known, Some(161));
    assert!(total.cached_input.invalid);
    assert!(total.reasoning_output.invalid && total.provider_total.invalid);
    assert_eq!(total.input_plus_output(), Some(240));
    total.add(None);
    assert_eq!(total.input.complete(), None);
    assert_eq!(total.input.known, Some(200));
    assert_eq!(total.input.unreported, 1);
    assert_eq!(total.input_plus_output(), None);
    let mut total = TokenTotals::default();
    let mut usage = observation();
    usage.usage.input_tokens = Some(u64::MAX);
    total.add(Some(usage));
    total.add(Some(observation()));
    assert_eq!(total.input.known, None);
    assert!(total.input.invalid);
    assert_eq!(total.input.complete(), None);
}

#[test]
fn repeated_serialized_restore_preserves_call_event_and_segment_sequences_without_recounting() {
    let mut original = fixture_tracker(
        "opaque-job",
        &[
            [Some(100), Some(60), Some(20), Some(10), Some(120)],
            [None; 5],
        ],
    );
    original.segment = 1;
    original.calls[1].segment = 1;
    original.calls[1].identity.model = "second-model".into();
    for _ in 0..3 {
        let restored: TokenTracker =
            serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
        assert_eq!(restored, original);
        assert_eq!(restored.event_sequence, 2);
        let totals = restored.totals();
        assert_eq!(totals.cumulative.input.known, Some(100));
        assert_eq!(totals.cumulative.input.unreported, 1);
        assert_eq!(totals.per_model.len(), 2);
        assert_eq!(totals.segments.len(), 2);
        assert_eq!(totals.segments[0].totals.input.complete(), Some(100));
        assert_eq!(totals.segments[1].totals.input.complete(), None);
        original = restored;
    }
}

struct FailAfterObservation(MemoryCheckpointStore);
impl CheckpointStore for FailAfterObservation {
    fn load(&self, job: &str) -> Result<Checkpoint, CheckpointError> {
        self.0.load(job)
    }
    fn acquire(&self, job: &str, new: bool) -> Result<String, CheckpointError> {
        self.0.acquire(job, new)
    }
    fn release(&self, job: &str, lease: &str, fenced: bool) {
        self.0.release(job, lease, fenced)
    }
    fn compare_and_save(
        &self,
        lease: &str,
        expected: u64,
        c: &Checkpoint,
    ) -> Result<SaveReceipt, CheckpointError> {
        if c.state
            .accounting
            .calls
            .iter()
            .any(|c| c.attempts.iter().any(|a| a.observation.is_some()))
        {
            Err(CheckpointError::Conflict)
        } else {
            self.0.compare_and_save(lease, expected, c)
        }
    }
}
#[test]
fn checkpoint_failure_keeps_live_observations_and_terminalizes_without_an_older_restore() {
    let ledger = JobAccounting::default();
    let store = Arc::new(FailAfterObservation(MemoryCheckpointStore::default()));
    let model = Model {
        mode: "success",
        cancelled: AtomicBool::new(false),
    };
    let error = SessionEngine::new(&model, &Runtime)
        .with_checkpoint_store(store.clone())
        .run(
            SessionBootstrap::new("prompt", limits(), "fixture").with_accounting(&ledger),
            &mut History::default(),
        )
        .unwrap_err();
    let PromptError::Interrupted { checkpoint, .. } = error else {
        panic!("latest checkpoint is carried")
    };
    assert_eq!(
        checkpoint.state.accounting.totals().cumulative.input.known,
        Some(100)
    );
    assert_eq!(
        store.load(&checkpoint.record.job),
        Err(CheckpointError::Fenced)
    );
    assert!(ledger.finalize(&DeliveryDisposition::Unknown));
    assert_eq!(ledger.snapshot().generation, CallOutcome::Failed);
    assert_eq!(ledger.snapshot().totals().cumulative.input.known, Some(100));
}

#[test]
fn report_deltas_come_only_from_the_tracker_and_restore_the_consume_cursor() {
    let store = Arc::new(MemoryCheckpointStore::default());
    let ledger = JobAccounting::default();
    ledger
        .install(
            fixture_tracker("opaque-job", &[[Some(4), None, Some(2), None, None]]),
            store.clone(),
        )
        .unwrap();
    let report = ledger.take_report().unwrap();
    assert_eq!(report.model_calls, 1);
    assert_eq!(report.input_tokens, 4);
    assert_eq!(report.cached_input_unreported_calls, 1);
    assert!(ledger.take_report().is_none());
    let saved = ledger.snapshot();
    let restored = JobAccounting::default();
    restored.install(saved, store).unwrap();
    assert!(restored.take_report().is_none());
}

#[test]
fn accounting_events_pin_typed_levels_fields_and_matching_span_parentage() {
    use tracing_subscriber::layer::SubscriberExt as _;
    let captured = dekopon_test_support::CaptureLayer::new();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    let job = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        let root = tracing::info_span!("host-message");
        let _root = root.enter();
        let ledger = JobAccounting::default();
        let model = Model {
            mode: "success",
            cancelled: AtomicBool::new(false),
        };
        SessionEngine::new(&model, &Runtime)
            .run(
                SessionBootstrap::new("private-prompt-sentinel", limits(), "fixture")
                    .with_accounting(&ledger),
                &mut History::default(),
            )
            .unwrap();
        let from = ledger.snapshot().calls[0].identity.clone();
        let before = ledger.snapshot().totals();
        for (i, outcome) in [
            TransitionOutcome::Denied,
            TransitionOutcome::AuthorizationFailed,
            TransitionOutcome::Applied,
        ]
        .into_iter()
        .enumerate()
        {
            ledger.transition(
                &TransitionRecord {
                    sequence: i as u32 + 1,
                    requesting_call: Some(1),
                    attempt: Some(i as u32 + 1),
                    control_id: None,
                    from: from.clone(),
                    requested: None,
                    to: None,
                    outcome,
                    decision_ref: None,
                    context_revision: 0,
                },
                before.clone(),
                std::time::Duration::ZERO,
            );
        }
        assert_eq!(ledger.snapshot().segment, 1);
        assert_eq!(
            ledger.snapshot().calls.len(),
            1,
            "refused controls invent no inference"
        );
        assert!(ledger.finalize(&DeliveryDisposition::Failed));
        assert!(!ledger.finalize(&DeliveryDisposition::Unknown));
        ledger.snapshot().job
    });
    let events = captured.events();
    let mut counts = [0; 3];
    for (fields, parent) in &events {
        for (i, event) in [
            "accounting.model.call",
            "accounting.model.transition",
            "accounting.model.job",
        ]
        .into_iter()
        .enumerate()
        {
            if fields.contains(&format!("audit.event=\"{event}\"")) {
                counts[i] += 1;
                assert_eq!(parent.as_deref(), Some(event));
                assert!(fields.contains(&format!("job.id={job}")), "{fields}");
                assert!(
                    fields.contains("accounting.version=1") && fields.contains("event.sequence=")
                );
                assert!(
                    fields.contains("accounting={"),
                    "typed JSON body missing: {fields}"
                );
            }
        }
    }
    assert_eq!(counts, [1, 3, 1]);
    let text = captured.events_text();
    assert!(!text.contains("private-prompt-sentinel"));
    assert!(!text.contains("accounting.model.turn"));
    assert!(
        text.contains("model.backend=adapter")
            && text.contains("model.name=fixture")
            && text.contains("model.effort=providerDefault")
    );
    assert!(
        text.contains("\"outcome\":\"denied\"")
            && text.contains("\"outcome\":\"authorizationFailed\"")
    );
    assert!(
        text.contains("\"before\":")
            && text.contains("\"per_model\":")
            && text.contains("\"cumulative\":")
    );
    assert!(text.contains("delivery=\"failed\""));
}
