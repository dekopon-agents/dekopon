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
        if !matches!(self.mode, "missing" | "supersede") {
            recorder.observe(attempt, observation())?;
            recorder.observe(attempt, observation())?; // identical delivery is idempotent
        }
        if self.mode == "conflict" {
            let mut other = observation();
            other.usage.input_tokens = Some(101);
            recorder.observe(attempt, other)?;
        }
        if self.mode == "supersede" {
            use dekopon_model::usage::ObservationPrecedence;
            // A streaming adapter reporting an early estimate, then the provider's own final
            // count, then one more interim event: the terminal report wins and is not displaced.
            let mut interim = observation();
            interim.usage.output_tokens = Some(1);
            recorder.observe_ranked(attempt, interim, ObservationPrecedence::Interim)?;
            recorder.observe_ranked(attempt, observation(), ObservationPrecedence::Final)?;
            recorder.observe_ranked(attempt, interim, ObservationPrecedence::Interim)?;
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
        "supersede",
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
            // A provider that reports usage twice, differently, no longer ends the job: the fields
            // it disagreed with itself about go unknown and the turn is delivered.
            matches!(mode, "success" | "missing" | "conflict" | "supersede"),
            "{mode}"
        );
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.calls.len(), 1);
        let total = snapshot.totals().cumulative;
        assert_eq!(total.attempts, 1);
        if mode == "missing" {
            assert_eq!(total.input.complete(), None);
            assert_eq!(total.input.unreported, 1);
        } else if mode == "conflict" {
            // Only `input_tokens` differed between the two reports, so only it is unknown.
            assert_eq!(total.input.complete(), None);
            assert_eq!(total.input.unreported, 1);
            assert!(total.input.invalid);
            assert_eq!(total.cached_input.known, Some(60));
            assert_eq!(total.output.known, Some(20));
            assert_eq!(total.reasoning_output.known, Some(10));
            assert_eq!(total.input_plus_output(), None);
        } else {
            // "supersede" lands here too: the terminal report's counts, not the interim ones.
            assert_eq!(total.input.known, Some(100));
            assert_eq!(total.cached_input.known, Some(60));
            assert_eq!(total.output.known, Some(20));
            assert_eq!(total.reasoning_output.known, Some(10));
            assert_eq!(total.input_plus_output(), Some(120));
        }
        assert!(!snapshot.invalid, "{mode} must not fence the ledger");
        // Nothing here fences the ledger for the rest of the job either.
        assert!(
            ledger
                .reserve(snapshot.calls[0].identity.clone(), CallKind::Image, 1)
                .is_ok(),
            "{mode} left the ledger unusable"
        );
        assert_eq!(
            snapshot.calls[0].outcome,
            if mode == "cancelled" {
                CallOutcome::Cancelled
            } else if mode == "model-error" {
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
fn one_inconsistent_field_is_unreported_without_blanking_the_rest_of_the_delta() {
    // `provider_total != input + output` is a real disagreement about the total — several
    // OpenAI-compatible endpoints define it differently — and it used to blank the whole delta
    // *after* the cursor had already moved past it, so the broker's live token view stayed empty
    // for the rest of the job.
    let store = Arc::new(MemoryCheckpointStore::default());
    let ledger = JobAccounting::default();
    ledger
        .install(
            fixture_tracker("opaque-job", &[[Some(4), None, Some(2), None, Some(99)]]),
            store.clone(),
        )
        .unwrap();
    let report = ledger.take_report().expect("the valid fields still report");
    assert_eq!(report.model_calls, 1);
    assert_eq!(report.input_tokens, 4);
    assert_eq!(report.input_unreported_calls, 0);
    assert_eq!(report.output_tokens, 2);
    assert_eq!(report.output_unreported_calls, 0);
    assert_eq!(report.total_tokens, 0);
    assert_eq!(report.total_unreported_calls, 1);
    // Advanced exactly once: the same delta is not offered twice.
    assert!(ledger.take_report().is_none());
}

#[test]
fn accounting_events_pin_typed_levels_fields_and_matching_span_parentage() {
    use tracing_subscriber::{Layer as _, layer::SubscriberExt as _};
    let captured = dekopon_test_support::CaptureLayer::new();
    let test_thread = std::thread::current().id();
    // tracing-core's single-dispatch fast path registers new callsites against the current
    // thread's dispatcher. A thread-local capture plus rebuild_interest_cache cannot prevent
    // a parallel, unsubscribed test from subsequently caching Interest::never. Install the
    // subscriber globally, but capture only this test's thread; sibling tests remain parallel
    // and cannot contaminate the exact accounting counts or retain their payloads here.
    let subscriber = tracing_subscriber::registry().with(captured.clone().with_filter(
        tracing_subscriber::filter::dynamic_filter_fn(move |_, _| {
            std::thread::current().id() == test_thread
        }),
    ));
    tracing::subscriber::set_global_default(subscriber)
        .expect("only global subscriber in this binary");
    let job = {
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
            TransitionOutcome::AuthorizationFailed {
                cause: crate::control::ControlFailureKind::Client(
                    dekopon_broker_protocol::ClientErrorKind::ControlBinding,
                ),
            },
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
    };
    for record in captured.records() {
        if let dekopon_test_support::Record::Event { level, target, .. } = record {
            assert_eq!(level, "INFO");
            assert_eq!(target, "dekopon_harness::audit");
        }
    }
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
    // The authorization failure carries *which* client failure produced it all the way into the
    // transition event, so an operator reading the audit stream can tell a substituted decision
    // binding from an unreachable broker.
    assert!(
        text.contains("\"outcome\":\"denied\"")
            && text
                .contains("\"authorizationFailed\":{\"cause\":{\"client\":\"control-binding\"}}"),
        "{text}"
    );
    assert!(
        text.contains("\"before\":")
            && text.contains("\"per_model\":")
            && text.contains("\"cumulative\":")
    );
    assert!(text.contains("delivery=\"failed\""));
}

#[test]
fn exported_calls_equal_tracker_totals_across_models_failures_images_and_missing_usage() {
    #[derive(Clone)]
    struct Writer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let known = [Some(100), Some(60), Some(20), Some(10), Some(120)];
    for missing in [false, true] {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = Writer(bytes.clone());
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let tracked = tracing::subscriber::with_default(subscriber, || {
            let mut tracker = fixture_tracker("export-job", &[known; 4]);
            tracker.calls[1].identity.model = "second-model".into();
            tracker.calls[1].identity.backend = "other-backend".into();
            tracker.calls[2].kind = CallKind::Image;
            tracker.calls[2].identity.model = "image-model".into();
            tracker.calls[2].model_turn = 2; // image and chat can share a model turn
            tracker.calls[3].model_turn = 3;
            tracker.calls[3].identity = tracker.calls[1].identity.clone();
            // A retry is another observed attempt, not another logical call or aggregation row.
            let mut retry = tracker.calls[1].attempts[0].clone();
            retry.sequence = 2;
            tracker.calls[1].attempts.push(retry);
            if missing {
                tracker.calls[0].attempts[0].observation = None;
                tracker.calls[2].attempts[0]
                    .observation
                    .as_mut()
                    .unwrap()
                    .usage
                    .total_tokens = None;
                tracker.calls[3].attempts.clear();
                tracker.calls[3].attempts_complete = false;
            }
            for call in &mut tracker.calls {
                call.event_sequence = None;
            }
            let mut live = LiveAccounting {
                tracker,
                span: None,
                store: None,
            };
            for sequence in 1..=4 {
                finish_call(
                    &mut live,
                    sequence,
                    if sequence == 1 || sequence == 3 {
                        CallOutcome::Failed
                    } else {
                        CallOutcome::Succeeded
                    },
                    "fixture",
                    0,
                    false,
                );
            }
            finalize(&mut live, &DeliveryDisposition::Failed);
            live.tracker.clone()
        });
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let mut records: Vec<serde_json::Value> = output
            .lines()
            .map(|line| {
                let event: serde_json::Value = serde_json::from_str(line).unwrap();
                let mut fields = event["fields"].clone();
                fields["trace_id"] = serde_json::json!("export");
                fields
            })
            .collect();
        assert_eq!(
            records
                .iter()
                .filter(|r| r["audit.event"] == "accounting.model.call")
                .count(),
            4
        );
        records.push(serde_json::json!({"trace_id":"export","audit.event":"agent.model.prompt","model.turn":1,"transcript.scope":"full","messages":"[{\"role\":\"user\",\"content\":\"request\"}]"}));
        records.extend(records.clone());
        records.reverse();
        let recorded = crate::replay::RecordedSession::from_records("export", &records).unwrap();
        assert!(
            recorded.turns.is_empty(),
            "failed calls need no assistant transcript"
        );
        assert_eq!(recorded.calls.as_ref().unwrap().len(), 4);
        assert_eq!(
            recorded
                .calls
                .as_ref()
                .unwrap()
                .iter()
                .filter(|c| c.kind == "image")
                .count(),
            1
        );
        assert_eq!(
            recorded.usage(),
            crate::replay::RecordedUsage::from(tracked.totals().cumulative.usage())
        );
        assert_eq!(tracked.totals().per_model.len(), 3);
        if missing {
            assert_eq!(recorded.usage(), crate::replay::RecordedUsage::default());
        } else {
            assert_eq!(recorded.usage().total_tokens, Some(600));
            assert_eq!(recorded.usage().input_tokens, Some(500));
            assert_eq!(recorded.usage().cached_input_tokens, Some(300));
        }
        let file = serde_json::to_vec(&recorded).unwrap();
        let decoded: crate::replay::RecordedSession = serde_json::from_slice(&file).unwrap();
        assert_eq!(decoded.usage(), recorded.usage());
    }
}
