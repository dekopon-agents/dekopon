use super::*;
use crate::{
    bootstrap::SessionBootstrap,
    conversation::{BoundedConversationStore, ConversationKey, ConversationWindow},
    history::{ExecutionOutcome, ExecutionProvenance, HistoryLimits},
    runtime::{ScriptRuntime, ShellRuntime},
    session::{CancellationProbe, PromptError, SessionEngine},
};
use dekopon_model::model::{
    AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool,
    ModelToolCall, ModelUsage,
};
use dekopon_shell::{CapabilityCallResult, CapabilityDescription, CapabilityInvoker, Limits};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Stop(AtomicBool);
impl CancellationProbe for Stop {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
struct Model {
    turns: Mutex<VecDeque<AssistantTurn>>,
    calls: AtomicUsize,
}
impl Model {
    fn new(turns: impl IntoIterator<Item = AssistantTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
            calls: AtomicUsize::new(0),
        }
    }
}
impl ChatModel for Model {
    fn complete(
        &self,
        _: &[ModelMessage],
        _: &[ModelTool],
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
        let result: Result<AssistantTurn, ModelError> = {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.turns
                .lock()
                .expect("turns")
                .pop_front()
                .ok_or(ModelError::NoChoices)
        };
        if let Ok(turn) = &result
            && let Some(usage) = turn.usage
        {
            recorder.observe(
                attempt,
                dekopon_model::usage::UsageObservation {
                    usage,
                    invalid: [false; 5],
                },
            )?;
        }
        result
    }
}
struct Invoker {
    count: AtomicUsize,
    stop: Option<Arc<Stop>>,
    fail: bool,
    persist_failure: Option<Arc<AtomicBool>>,
}
impl CapabilityInvoker for Invoker {
    fn granted(&self) -> Vec<String> {
        vec!["test.read".to_owned()]
    }
    fn describe(&self, _: &str) -> Option<CapabilityDescription> {
        Some(CapabilityDescription {
            capability: "test.read".to_owned(),
            description: "fixture".to_owned(),
            input_schema: json!({"type":"object"}),
        })
    }
    fn invoke(
        &self,
        _: &str,
        _: Value,
        _: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        self.count.fetch_add(1, Ordering::SeqCst);
        if let Some(stop) = &self.stop {
            stop.0.store(true, Ordering::SeqCst);
        }
        if let Some(fail) = &self.persist_failure {
            fail.store(true, Ordering::SeqCst);
        }
        if self.fail {
            CapabilityCallResult::Failed {
                error: "failed after work".to_owned(),
            }
        } else {
            CapabilityCallResult::Succeeded(json!({"observed":"fixture evidence"}))
        }
    }
}
fn runtime() -> ShellRuntime<Invoker> {
    ShellRuntime {
        invoker: Invoker {
            count: AtomicUsize::new(0),
            stop: None,
            fail: false,
            persist_failure: None,
        },
        limits: Limits::default(),
        curl_capability: None,
    }
}
fn limits() -> PromptLimits {
    PromptLimits {
        max_steps: 4,
        max_capability_calls: 4,
    }
}
fn script(text: &str) -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: "call-a".to_owned(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: "bash".to_owned(),
                arguments: json!({"script":text}).to_string(),
            },
        }],
        usage: Some(ModelUsage {
            input_tokens: Some(10),
            cached_input_tokens: Some(3),
            output_tokens: Some(4),
            reasoning_output_tokens: Some(1),
            total_tokens: Some(14),
        }),
        replay_items: vec![json!({"encrypted_content":"opaque-never-portable"})],
    }
}
fn answer() -> AssistantTurn {
    AssistantTurn {
        content: Some("generated, not delivered".to_owned()),
        tool_calls: Vec::new(),
        usage: None,
        replay_items: Vec::new(),
    }
}
fn snapshot() -> Checkpoint {
    let record = JobRecord::unanswered("request");
    let accounting = crate::accounting::fixture_tracker(&record.job, &[]);
    Checkpoint {
        version: CHECKPOINT_VERSION,
        revision: 0,
        position: Position::Ready,
        scope: "scope".to_owned(),
        surface: "surface".to_owned(),
        model: "fixture".to_owned(),
        effort: "providerDefault".to_owned(),
        context_revision: 0,
        record,
        history: History::default(),
        limits: limits(),
        state: SessionState {
            accounting,
            ..SessionState::default()
        },
        pending_execution: None,
        finalized: false,
    }
}

#[test]
fn nested_execution_and_budget_evidence_survive_success_and_final_inference_failure() {
    for final_answer in [false, true] {
        let mut turns = vec![script("echo builtin; cap --list; test.read; test.read")];
        if final_answer {
            turns.push(answer());
        }
        let model = Model::new(turns);
        let runtime = runtime();
        let store = Arc::new(MemoryCheckpointStore::default());
        let mut history = History::default();
        let result = SessionEngine::new(&model, &runtime)
            .with_checkpoint_store(store.clone())
            .run(
                SessionBootstrap::new("request", limits(), "fixture"),
                &mut history,
            );
        assert_eq!(result.is_ok(), final_answer);
        assert_eq!(runtime.invoker.count.load(Ordering::SeqCst), 2);
        let record = &history.turns()[0];
        assert_eq!(
            record.executions.len(),
            2,
            "builtins and help are not executions"
        );
        assert!(record.groups[0].complete());
        assert_eq!(record.delivery, DeliveryDisposition::Pending);
        for (index, execution) in record.executions.iter().enumerate() {
            assert_eq!(execution.sequence as usize, index + 1);
            assert_eq!(execution.call, 1);
            assert_eq!(execution.tool, "call-a");
            assert_eq!(execution.job, record.job);
            assert_eq!(execution.provenance, ExecutionProvenance::DirectReadOnly);
            assert_eq!(execution.outcome, ExecutionOutcome::Succeeded);
            assert!(
                execution
                    .result
                    .as_ref()
                    .expect("excerpt")
                    .text
                    .contains("fixture evidence")
            );
        }
        let saved = store.load(&record.job).expect("saved latest");
        assert_eq!(saved.state.spent.capability_invocations, 2);
        assert_eq!(saved.state.spent.model_calls, 2);
        assert_eq!(
            saved
                .state
                .accounting
                .calls
                .iter()
                .map(|c| c.attempts[0].observation.unwrap_or_default().usage.fields())
                .collect::<Vec<_>>(),
            vec![[Some(10), Some(3), Some(4), Some(1), Some(14)], [None; 5]]
        );
        let encoded = serde_json::to_string(&saved).expect("checkpoint JSON");
        assert!(!encoded.contains("opaque-never-portable"));
        let decoded: Checkpoint = serde_json::from_str(&encoded).expect("strict restore shape");
        assert_eq!(decoded, saved);
    }
}

#[test]
fn failed_capability_and_stop_keep_observed_outcomes_before_cancellation_checks() {
    let mut runtime = runtime();
    runtime.invoker.fail = true;
    let stop = Arc::new(Stop::default());
    runtime.invoker.stop = Some(stop.clone());
    let model = Model::new([script("test.read; test.read"), answer()]);
    let mut history = History::default();
    let result = SessionEngine::new(&model, &runtime).run(
        SessionBootstrap::new("request", limits(), "fixture").with_cancellation(stop.as_ref()),
        &mut history,
    );
    assert!(matches!(result, Err(PromptError::Cancelled)));
    assert_eq!(
        runtime.invoker.count.load(Ordering::SeqCst),
        1,
        "no dispatch after Stop"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let record = &history.turns()[0];
    assert_eq!(record.executions[0].outcome, ExecutionOutcome::Failed);
    assert_eq!(record.delivery, DeliveryDisposition::Cancelled);
    assert!(record.generated.is_none());
}

struct FailingStore {
    inner: MemoryCheckpointStore,
    failed: Arc<AtomicBool>,
    fail_before_dispatch: bool,
}
impl CheckpointStore for FailingStore {
    fn load(&self, job: &str) -> Result<Checkpoint, CheckpointError> {
        self.inner.load(job)
    }
    fn acquire(&self, job: &str, new: bool) -> Result<String, CheckpointError> {
        self.inner.acquire(job, new)
    }
    fn compare_and_save(
        &self,
        lease: &str,
        revision: u64,
        c: &Checkpoint,
    ) -> Result<SaveReceipt, CheckpointError> {
        if self.failed.load(Ordering::SeqCst)
            || (self.fail_before_dispatch && c.position == Position::DispatchPending)
        {
            Err(CheckpointError::Conflict)
        } else {
            self.inner.compare_and_save(lease, revision, c)
        }
    }
    fn release(&self, job: &str, lease: &str, fenced: bool) {
        self.inner.release(job, lease, fenced);
    }
}
#[test]
fn failed_pre_dispatch_save_executes_nothing_and_failed_post_save_retains_live_facts() {
    for before in [true, false] {
        let failure = Arc::new(AtomicBool::new(false));
        let store = Arc::new(FailingStore {
            inner: MemoryCheckpointStore::default(),
            failed: failure.clone(),
            fail_before_dispatch: before,
        });
        let mut runtime = runtime();
        runtime.invoker.persist_failure = Some(failure);
        let model = Model::new([script("test.read; test.read"), answer()]);
        let mut history = History::default();
        let error = SessionEngine::new(&model, &runtime)
            .with_checkpoint_store(store.clone())
            .run(
                SessionBootstrap::new("request", limits(), "fixture"),
                &mut history,
            )
            .expect_err("save failure halts");
        let PromptError::Interrupted { source, checkpoint } = error else {
            panic!("latest live checkpoint must accompany persistence failure");
        };
        assert_eq!(source, CheckpointError::Conflict);
        assert_eq!(
            runtime.invoker.count.load(Ordering::SeqCst),
            usize::from(!before)
        );
        assert_eq!(
            checkpoint.record.executions[0].outcome,
            if before {
                ExecutionOutcome::NotExecuted
            } else {
                ExecutionOutcome::Succeeded
            }
        );
        assert_eq!(checkpoint.state.spent.capability_invocations, 1);
        assert!(matches!(
            store.load(&checkpoint.record.job),
            Err(CheckpointError::Fenced)
        ));
    }
}

/// The lease ceiling and the byte ceiling agree, and neither destroys a resumable checkpoint.
///
/// `MAX_STORE_BYTES` used to be a quarter of `MAX_JOBS` reservations, so the store silently
/// stopped at 32 concurrent sessions — and it got there destructively: with the byte ceiling full
/// of leases, the eviction loop drained *every* unleased checkpoint and then returned `Capacity`
/// anyway, so the refusal that failed this message also destroyed the snapshots the other
/// in-flight messages were going to resume from.
#[test]
fn concurrent_leases_reach_the_job_ceiling_without_destroying_stored_checkpoints() {
    let store = Arc::new(MemoryCheckpointStore::default());
    let stored = snapshot();
    let resumable = stored.record.job.clone();
    let lease = store.acquire(&resumable, true).expect("fixture lease");
    store
        .compare_and_save(&lease, 0, &stored)
        .expect("fixture checkpoint");
    store.release(&resumable, &lease, false);

    let mut live = Vec::new();
    for index in 1..MAX_JOBS {
        let job = opaque_id();
        let lease = store
            .acquire(&job, true)
            .unwrap_or_else(|error| panic!("lease {index} of {MAX_JOBS}: {error}"));
        live.push((job, lease));
        assert!(
            store.load(&resumable).is_ok(),
            "lease {index} evicted a dormant snapshot it did not need"
        );
    }
    // The store now holds MAX_JOBS entries: one dormant snapshot and MAX_JOBS - 1 live leases. A
    // live session outranks a dormant snapshot, so this one is admitted by evicting exactly it.
    let last = opaque_id();
    let lease = store.acquire(&last, true).expect("the ceiling itself");
    live.push((last, lease));
    assert!(matches!(
        store.load(&resumable),
        Err(CheckpointError::NotFound)
    ));

    let refusal = store.acquire(&opaque_id(), true);
    assert_eq!(refusal, Err(CheckpointError::Capacity));
    assert!(
        refusal
            .unwrap_err()
            .to_string()
            .contains(&MAX_JOBS.to_string()),
        "the refusal names the ceiling it hit"
    );
    for (job, lease) in live {
        store.release(&job, &lease, false);
    }
}

/// A refused blank answer is never recorded as an answer, so a resume cannot deliver one.
///
/// The whitespace-only rejection happens after the generated text is written to the checkpoint, so
/// the failing job left a record claiming an answer of `"   "`. The conversation this job is
/// appended to then replays a blank assistant turn, and a resumed job hands the transport an empty
/// `Send` — `SessionExit::answer` is documented empty only when the disposition is `Suppress`.
#[test]
fn a_blank_answer_is_neither_recorded_nor_resumed_as_a_delivered_one() {
    let store = Arc::new(MemoryCheckpointStore::default());
    let runtime = runtime();
    let blank = AssistantTurn {
        content: Some("   \n".to_owned()),
        tool_calls: Vec::new(),
        usage: None,
        replay_items: Vec::new(),
    };
    let model = Model::new([blank]);
    let mut history = History::default();
    let error = SessionEngine::new(&model, &runtime)
        .with_checkpoint_store(store.clone())
        .run(
            SessionBootstrap::new("request", limits(), "fixture").with_scope("scope"),
            &mut history,
        )
        .expect_err("a blank answer is not an answer");
    assert!(matches!(error, PromptError::EmptyAnswer));

    let recorded = history.turns().last().expect("the failed job is recorded");
    assert_eq!(
        recorded.answer(),
        None,
        "the conversation must not claim this turn was answered"
    );
    let job = recorded.job.clone();
    let saved = store
        .load(&job)
        .expect("the checkpoint outlived the failure");
    assert_eq!(saved.record.generated, None);

    // Resuming that job surfaces the failure rather than a blank `Send`: the finished generation
    // fenced the lease, and a resume that fabricated an answer out of it would be worse than one
    // that refuses. Nothing in this path can hand a transport an empty answer to deliver.
    let model = Model::new([answer()]);
    let error = SessionEngine::new(&model, &runtime)
        .with_checkpoint_store(store)
        .run(
            SessionBootstrap::new("request", limits(), "fixture")
                .with_scope("scope")
                .with_resume(&job),
            &mut history,
        )
        .expect_err("a fenced job is not resumable");
    assert!(matches!(
        error,
        PromptError::Checkpoint(CheckpointError::Fenced)
    ));
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        0,
        "no resumed inference"
    );
}

#[test]
fn unknown_work_fences_later_dispatch_and_restore_even_after_history_trimming() {
    let store = Arc::new(MemoryCheckpointStore::default());
    let journal = ExecutionJournal::new(store.clone(), snapshot(), true, None).expect("lease");
    let id = journal.reserve("test.read").expect("reserve");
    journal
        .observe(id, |r| r.outcome = ExecutionOutcome::Unknown)
        .expect("observation saved");
    assert_eq!(
        journal.reserve("test.read"),
        Err(CheckpointError::UnknownWork)
    );
    let saved = journal.snapshot();
    assert_eq!(
        saved.validate_resume("scope", "surface"),
        Err(CheckpointError::UnknownWork)
    );
    let mut history = History::new(HistoryLimits {
        max_turns: 0,
        max_bytes: 0,
    });
    history.record(saved.record);
    assert!(history.is_empty() && history.has_unknown_work());
    let mut context = Vec::new();
    history.replay_into(&mut context);
    assert!(
        context[0]
            .content()
            .expect("warning")
            .contains("Do not resubmit")
    );
}

#[test]
fn latest_restore_keeps_job_usage_sequences_and_budgets_without_replaying_effects() {
    let runtime = runtime();
    let store = Arc::new(MemoryCheckpointStore::default());
    let mut saved = snapshot();
    saved.surface = runtime
        .capability_snapshot()
        .expect("surface")
        .fingerprint();
    saved.state.spent.model_calls = 1;
    saved.state.spent.capability_invocations = 3;
    saved.state.spent.script_calls = 1;
    saved.state.spent.control_attempts = 2;
    saved.state.accounting = crate::accounting::fixture_tracker(
        &saved.record.job,
        &[[Some(7), None, Some(2), None, None]],
    );
    let job = saved.record.job.clone();
    let lease = store.acquire(&job, true).expect("fixture lease");
    store
        .compare_and_save(&lease, 0, &saved)
        .expect("fixture checkpoint");
    store.release(&job, &lease, false);
    let model = Model::new([answer()]);
    let mut history = History::default();
    let result = SessionEngine::new(&model, &runtime)
        .with_checkpoint_store(store.clone())
        .run(
            SessionBootstrap::new("request", limits(), "fixture")
                .with_scope("scope")
                .with_resume(&job),
            &mut history,
        )
        .expect("resume latest");
    assert_eq!(result.job, job);
    assert_eq!(result.model_turns, 2);
    assert_eq!(result.capability_invocations, 3);
    assert_eq!(runtime.invoker.count.load(Ordering::SeqCst), 0);
    let saved = store.load(&job).expect("latest");
    assert_eq!(saved.state.accounting.calls.len(), 2);
    assert_eq!(
        saved.state.accounting.calls[0].attempts[0]
            .observation
            .unwrap()
            .usage
            .fields(),
        [Some(7), None, Some(2), None, None]
    );
    assert_eq!(saved.state.spent.control_attempts, 2);
    assert_eq!(saved.context_revision, 1);
}

#[test]
fn checkpoint_version_scope_capacity_cas_and_exclusive_live_lease_fail_explicitly() {
    let store = Arc::new(MemoryCheckpointStore::default());
    let saved = snapshot();
    assert_eq!(
        saved.validate_resume("other", "surface"),
        Err(CheckpointError::ScopeChanged)
    );
    let mut invalid = saved.clone();
    invalid.version += 1;
    assert_eq!(
        invalid.validate_resume("scope", "surface"),
        Err(CheckpointError::Invalid)
    );
    let mut attempted = saved.clone();
    attempted.state.image_generation_attempted = true;
    assert_eq!(
        attempted.validate_resume("scope", "surface"),
        Err(CheckpointError::AssetsUnavailable)
    );
    let mut invalid = saved.clone();
    invalid.state.spent.capability_invocations = 5;
    assert_eq!(
        invalid.validate_resume("scope", "surface"),
        Err(CheckpointError::Invalid)
    );
    let lease = store.acquire(&saved.record.job, true).expect("first lease");
    assert_eq!(
        store.acquire(&saved.record.job, false),
        Err(CheckpointError::Active)
    );
    let receipt = store
        .compare_and_save(&lease, 0, &saved)
        .expect("first save");
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        store.compare_and_save(&lease, 0, &saved),
        Err(CheckpointError::Conflict)
    );
    let mut active = vec![(saved.record.job, lease)];
    for _ in 1..MAX_JOBS {
        let job = opaque_id();
        let lease = store.acquire(&job, true).expect("reserved bounded slot");
        active.push((job, lease));
    }
    let refusal = store.acquire(&opaque_id(), true);
    assert_eq!(refusal, Err(CheckpointError::Capacity));
    assert!(
        refusal
            .unwrap_err()
            .to_string()
            .contains(&MAX_JOBS.to_string()),
        "the refusal names the ceiling it hit"
    );
    for (job, lease) in active {
        store.release(&job, &lease, false);
    }
}

#[test]
fn scoped_generation_leases_fence_aba_refusal_eviction_and_late_append() {
    let store = BoundedConversationStore::new(2);
    let now = Instant::now();
    let window = ConversationWindow {
        idle_timeout: Duration::from_secs(10),
        limits: HistoryLimits::default(),
    };
    let key = ConversationKey::scoped("agent", "route", "transport", "channel", "thread", "sender");
    let other =
        ConversationKey::scoped("agent", "route", "transport", "channel", "thread", "other");
    let a = vec!["metadata-a-epoch-one".to_owned()];
    let b = vec!["metadata-b-epoch-two".to_owned()];
    let first = store.begin(&key, &a, window, now);
    let concurrent = store.begin(&key, &a, window, now);
    store
        .commit(
            &key,
            &a,
            window,
            JobRecord::unanswered("one"),
            &first.cache_key,
            now,
        )
        .expect("append");
    store
        .commit(
            &key,
            &a,
            window,
            JobRecord::unanswered("two"),
            &concurrent.cache_key,
            now,
        )
        .expect("append, not overwrite");
    assert_eq!(store.begin(&key, &a, window, now).history.len(), 2);
    assert!(store.begin(&other, &a, window, now).history.is_empty());
    let second = store.begin(&key, &b, window, now);
    let third = store.begin(&key, &a, window, now);
    assert_ne!(first.cache_key, second.cache_key);
    assert_ne!(first.cache_key, third.cache_key);
    assert!(
        store
            .commit(
                &key,
                &a,
                window,
                JobRecord::unanswered("late"),
                &first.cache_key,
                now
            )
            .is_err()
    );
    store.remove(&key, crate::conversation::EvictionReason::GrantChanged);
    assert!(
        store
            .commit(
                &key,
                &a,
                window,
                JobRecord::unanswered("refused"),
                &third.cache_key,
                now
            )
            .is_err()
    );
    let fresh = store.begin(&key, &a, window, now);
    let expired = store.begin(&key, &a, window, now + Duration::from_secs(11));
    assert_ne!(fresh.cache_key, expired.cache_key);
}

#[test]
fn excerpts_and_whole_batches_are_bounded_and_delivery_is_not_generation() {
    let text = "é".repeat(4096);
    let excerpt = Excerpt::new(&text, MAX_EXCERPT_BYTES);
    assert_eq!(excerpt.text.len(), MAX_EXCERPT_BYTES);
    assert_eq!(excerpt.original_bytes, 8192);
    assert!(excerpt.truncated);
    assert_eq!(excerpt.digest.len(), 64);
    let mut history = History::default();
    let mut record = JobRecord::completed("request", "long generated answer");
    record.delivery = DeliveryDisposition::Accepted {
        text: "exact bounded accepted text".to_owned(),
    };
    history.record(record);
    let mut context = Vec::new();
    history.replay_into(&mut context);
    assert!(context.iter().any(|m| {
        m.content()
            .is_some_and(|t| t.contains("exact bounded accepted text"))
    }));
    let mut messages = vec![ModelMessage::user("never trim the inbound request")];
    let turn = script("test.read");
    messages.push(dekopon_model::model::assistant_message(&turn));
    messages.push(ModelMessage::tool(
        "call-a",
        "x".repeat(crate::context::MAX_GROUP_BYTES),
    ));
    assert!(crate::context::bound_live(&mut messages).expect("trim entire batch"));
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].content(),
        Some("never trim the inbound request")
    );
}

#[test]
fn invalid_names_and_terminal_validation_failures_cannot_pin_checkpoint_capacity() {
    let store = Arc::new(MemoryCheckpointStore::default());
    let runtime = runtime();
    let mut jobs = Vec::new();
    for _ in 0..MAX_JOBS {
        let model = Model::new([script(&format!("cap {}", "a".repeat(257)))]);
        let mut history = History::default();
        assert!(
            SessionEngine::new(&model, &runtime)
                .with_checkpoint_store(store.clone())
                .run(
                    SessionBootstrap::new("request", limits(), "fixture"),
                    &mut history
                )
                .is_err()
        );
        let record = &history.turns()[0];
        assert!(record.executions.is_empty());
        jobs.push(record.job.clone());
    }
    assert_eq!(runtime.invoker.count.load(Ordering::SeqCst), 0);
    SessionEngine::new(&Model::new([answer()]), &runtime)
        .with_checkpoint_store(store.clone())
        .run(
            SessionBootstrap::new("valid", limits(), "fixture"),
            &mut History::default(),
        )
        .unwrap();
    assert!(jobs.iter().all(|job| matches!(
        store.load(job),
        Err(CheckpointError::NotFound | CheckpointError::Fenced)
    )));
    for _ in 0..MAX_JOBS {
        let mut saved = snapshot();
        saved.record.user = "x".repeat(128 * 1024 + 1);
        assert!(ExecutionJournal::new(store.clone(), saved, true, None).is_err());
    }
    SessionEngine::new(&Model::new([answer()]), &runtime)
        .with_checkpoint_store(store)
        .run(
            SessionBootstrap::new("valid again", limits(), "fixture"),
            &mut History::default(),
        )
        .unwrap();
}

#[test]
fn repeated_provider_ids_bind_only_their_own_batch_results_and_portable_ids_are_unique() {
    let runtime = runtime();
    let store = Arc::new(MemoryCheckpointStore::default());
    let mut history = History::default();
    let model = Model::new([
        script("echo first-success"),
        script("echo second-denial; false"),
        answer(),
    ]);
    let exit = SessionEngine::new(&model, &runtime)
        .with_checkpoint_store(store.clone())
        .run(
            SessionBootstrap::new("first job", limits(), "fixture"),
            &mut history,
        )
        .unwrap();
    let saved = store.load(&exit.job).unwrap();
    assert!(
        saved.record.groups[0].results[0]
            .result
            .text
            .contains("first-success")
    );
    assert!(
        saved.record.groups[1].results[0]
            .result
            .text
            .contains("second-denial")
    );
    assert!(
        !saved.record.groups[1].results[0]
            .result
            .text
            .contains("first-success")
    );
    let second = Model::new([script("echo another-job"), answer()]);
    SessionEngine::new(&second, &runtime)
        .with_checkpoint_store(store)
        .run(
            SessionBootstrap::new("second job", limits(), "fixture"),
            &mut history,
        )
        .unwrap();
    let mut context = Vec::new();
    history.replay_into(&mut context);
    let ids: Vec<_> = context
        .iter()
        .filter(|m| m.role() == "tool")
        .map(|m| {
            serde_json::to_value(m).unwrap()["tool_call_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
        3
    );
}

#[test]
fn resume_uses_saved_history_once_not_the_callers_empty_seed() {
    struct Inspect;
    impl ChatModel for Inspect {
        fn complete(
            &self,
            messages: &[ModelMessage],
            _: &[ModelTool],
            recorder: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<AssistantTurn, ModelError> {
            recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            for text in ["previous request", "previous answer", "current request"] {
                assert_eq!(
                    messages
                        .iter()
                        .filter(|m| m.content() == Some(text))
                        .count(),
                    1,
                    "{text}"
                );
            }
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| m.role() == "tool" && m.content() == Some("prior-result"))
                    .count(),
                1
            );
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| m.content().is_some_and(|s| s.contains("execution-excerpt")))
                    .count(),
                1
            );
            Ok(answer())
        }
    }
    let runtime = runtime();
    let store = Arc::new(MemoryCheckpointStore::default());
    let mut saved = snapshot();
    saved.surface = runtime.capability_snapshot().unwrap().fingerprint();
    saved.record.user = "current request".to_owned();
    let mut prior = JobRecord::completed("previous request", "previous answer");
    prior.groups.push(crate::history::ToolGroup {
        call: 1,
        calls: script("echo prior-result").tool_calls,
        results: vec![crate::history::ToolResult {
            id: "call-a".into(),
            result: Excerpt::new("prior-result", MAX_EXCERPT_BYTES),
        }],
        omitted: false,
        provenance: None,
    });
    prior.executions.push(ExecutionRecord {
        job: prior.job.clone(),
        call: 1,
        tool: "call-a".into(),
        sequence: 1,
        capability: "test.read".into(),
        provenance: ExecutionProvenance::DirectReadOnly,
        invocation: None,
        evidence: vec![],
        outcome: ExecutionOutcome::Succeeded,
        result: Some(Excerpt::new("execution-excerpt", MAX_EXCERPT_BYTES)),
    });
    saved.history.record(prior);
    let job = saved.record.job.clone();
    let lease = store.acquire(&job, true).unwrap();
    store.compare_and_save(&lease, 0, &saved).unwrap();
    store.release(&job, &lease, false);
    SessionEngine::new(&Inspect, &runtime)
        .with_checkpoint_store(store)
        .run(
            SessionBootstrap::new("ignored", limits(), "fixture")
                .with_scope("scope")
                .with_resume(&job),
            &mut History::default(),
        )
        .unwrap();
}

#[test]
fn unfinished_batch_reusing_a_provider_id_cannot_capture_earlier_success() {
    let first = script("echo first");
    let second = script("echo denied");
    let messages = vec![
        dekopon_model::model::assistant_message(&first),
        ModelMessage::tool("call-a", "old success"),
        dekopon_model::model::assistant_message(&second),
    ];
    let mut group = crate::history::ToolGroup {
        call: 2,
        calls: second.tool_calls,
        results: vec![],
        omitted: false,
        provenance: None,
    };
    group.capture_results(&messages);
    assert!(group.results.is_empty());
    assert!(!group.complete());
}
