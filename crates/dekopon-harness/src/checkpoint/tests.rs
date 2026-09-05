use super::*;
use crate::{
    bootstrap::SessionBootstrap,
    checkpoint::FinalState,
    conversation::{BoundedConversationStore, ConversationKey, ConversationWindow},
    history::{ExecutionOutcome, ExecutionProvenance, HistoryLimits},
    runtime::ShellRuntime,
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
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
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
    /// Freshness checks to admit before the host surface is reported as changed.
    ///
    /// The real fence: the engine re-checks the surface at every turn boundary, and a broker whose
    /// policy moved underneath a running session is what makes that check fail.
    fresh_checks: Option<AtomicUsize>,
}
impl CapabilityInvoker for Invoker {
    fn check_freshness(&self) -> Result<(), dekopon_shell::FreshnessError> {
        match &self.fresh_checks {
            Some(remaining) if remaining.fetch_sub(1, Ordering::SeqCst) == 0 => Err(
                dekopon_shell::FreshnessError::Unavailable("fixture".to_owned()),
            ),
            _ => Ok(()),
        }
    }
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
            fresh_checks: None,
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
        let final_state = FinalState::default();
        let mut history = History::default();
        let result = SessionEngine::new(&model, &runtime).run(
            SessionBootstrap::new("request", limits(), "fixture").with_final_state(&final_state),
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
        let saved = final_state
            .take()
            .expect("the session published its final state");
        assert_eq!(saved.record.job, record.job);
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
        // The provider's opaque continuation is request-local and never reaches portable state.
        let portable = format!(
            "{}{}",
            serde_json::to_string(&saved.record).expect("the record is portable"),
            serde_json::to_string(&saved.state).expect("the loop state is portable"),
        );
        assert!(!portable.contains("opaque-never-portable"));
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

/// A fence after dispatch keeps every observed outcome and stops before the next model turn.
///
/// The engine re-checks the host surface at each turn boundary, and a broker whose policy moved
/// under a running session fails that check. Nothing about the work already done is rolled back:
/// the capability ran, its outcome is recorded, and the session hands the whole live state back
/// rather than an older copy of it.
#[test]
fn a_fence_after_dispatch_retains_live_facts_and_stops_the_session() {
    let mut runtime = runtime();
    // Admit the checks this session makes before and after its first answer, then report the
    // surface as gone at the boundary of the second turn.
    runtime.invoker.fresh_checks = Some(AtomicUsize::new(2));
    let model = Model::new([script("test.read"), answer()]);
    let mut history = History::default();
    let error = SessionEngine::new(&model, &runtime)
        .run(
            SessionBootstrap::new("request", limits(), "fixture"),
            &mut history,
        )
        .expect_err("a fenced surface halts the session");
    let PromptError::Interrupted { source, checkpoint } = error else {
        panic!("the latest live state must accompany a fence");
    };
    assert_eq!(source, CheckpointError::ScopeChanged);
    assert_eq!(runtime.invoker.count.load(Ordering::SeqCst), 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1, "no fenced inference");
    assert_eq!(
        checkpoint.record.executions[0].outcome,
        ExecutionOutcome::Succeeded
    );
    assert_eq!(checkpoint.state.spent.capability_invocations, 1);
    assert_eq!(
        history.turns()[0].executions[0].outcome,
        ExecutionOutcome::Succeeded,
        "the recorded turn keeps what the fence interrupted"
    );
}

/// A refused blank answer is never recorded as an answer.
///
/// The whitespace-only rejection happens after the generated text is written to the live state, so
/// the failing job left a record claiming an answer of `"   "`. The conversation this job is
/// appended to then replays a blank assistant turn, and the transport is handed an empty `Send` —
/// `SessionExit::answer` is documented empty only when the disposition is `Suppress`.
#[test]
fn a_blank_answer_is_never_recorded_as_a_delivered_one() {
    let runtime = runtime();
    let blank = AssistantTurn {
        content: Some("   \n".to_owned()),
        tool_calls: Vec::new(),
        usage: None,
        replay_items: Vec::new(),
    };
    let model = Model::new([blank]);
    let final_state = FinalState::default();
    let mut history = History::default();
    let error = SessionEngine::new(&model, &runtime)
        .run(
            SessionBootstrap::new("request", limits(), "fixture")
                .with_scope("scope")
                .with_final_state(&final_state),
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
    assert_eq!(
        final_state
            .take()
            .expect("the session published its final state")
            .record
            .generated,
        None
    );
}

#[test]
fn unknown_work_fences_later_dispatch_even_after_history_trimming() {
    let journal = ExecutionJournal::new(snapshot(), None).expect("journal opens");
    let id = journal.reserve("test.read").expect("reserve");
    journal
        .observe(id, |r| r.outcome = ExecutionOutcome::Unknown)
        .expect("observation saved");
    assert_eq!(
        journal.reserve("test.read"),
        Err(CheckpointError::UnknownWork)
    );
    let saved = journal.snapshot();
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

/// Every field bound is enforced on the mutation that broke it, and the break fences the job.
///
/// The bounds are the whole of what makes the live state trustworthy — a spend past the session's
/// own ceiling, a control scope naming another job, an identity disagreeing with the model the
/// turn is running under — so each one is checked after every mutation rather than at some
/// terminal point a fenced session never reaches.
#[test]
fn a_mutation_that_breaks_a_field_bound_is_refused_and_fences_the_job() {
    let journal = ExecutionJournal::new(snapshot(), None).expect("journal opens");
    assert_eq!(
        journal.update(|c| c.state.spent.capability_invocations = 5),
        Err(CheckpointError::Invalid),
        "a spend past the session ceiling is not a valid state"
    );
    assert_eq!(
        journal.update(|c| c.state.spent.capability_invocations = 0),
        Err(CheckpointError::Invalid),
        "the fence is sticky; a later well-formed mutation does not clear it"
    );
    assert_eq!(
        journal.snapshot().state.spent.capability_invocations,
        0,
        "the mutation is still applied: a fenced job keeps observing"
    );

    for (name, break_it) in [
        (
            "effort",
            Box::new(|c: &mut Checkpoint| c.effort = "exhaustive".to_owned())
                as Box<dyn Fn(&mut Checkpoint)>,
        ),
        (
            "control attempts",
            Box::new(|c: &mut Checkpoint| c.state.spent.control_attempts = 5),
        ),
        (
            "execution job",
            Box::new(|c: &mut Checkpoint| c.record.executions[0].job = "other".to_owned()),
        ),
        (
            "model calls",
            Box::new(|c: &mut Checkpoint| c.state.spent.model_calls = c.limits.max_steps + 1),
        ),
    ] {
        let journal = ExecutionJournal::new(snapshot(), None).expect("journal opens");
        journal.reserve("test.read").expect("reservation");
        assert_eq!(
            journal.update(&break_it),
            Err(CheckpointError::Invalid),
            "{name}"
        );
    }

    let mut oversized = snapshot();
    oversized.record.user = "x".repeat(128 * 1024 + 1);
    assert_eq!(
        ExecutionJournal::new(oversized, None).err(),
        Some(CheckpointError::Invalid),
        "an invalid opening state never opens a journal at all"
    );
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

/// A model-authored capability name that is not one is refused before any dispatch.
///
/// The name reaches the journal straight from model output, so an escape-hatch string must fence
/// the reservation rather than land in the ledger: nothing is invoked, the execution ledger stays
/// empty, and the next session is unaffected by the one that tried.
#[test]
fn an_invalid_capability_name_is_refused_without_dispatch_or_a_ledger_entry() {
    let runtime = runtime();
    let model = Model::new([script(&format!("cap {}", "a".repeat(257)))]);
    let mut history = History::default();
    assert!(
        SessionEngine::new(&model, &runtime)
            .run(
                SessionBootstrap::new("request", limits(), "fixture"),
                &mut history
            )
            .is_err()
    );
    assert!(history.turns()[0].executions.is_empty());
    assert_eq!(runtime.invoker.count.load(Ordering::SeqCst), 0);
    SessionEngine::new(&Model::new([answer()]), &runtime)
        .run(
            SessionBootstrap::new("valid", limits(), "fixture"),
            &mut History::default(),
        )
        .expect("the refused name fenced its own job and nothing else");
}

#[test]
fn repeated_provider_ids_bind_only_their_own_batch_results_and_portable_ids_are_unique() {
    let runtime = runtime();
    let final_state = FinalState::default();
    let mut history = History::default();
    let model = Model::new([
        script("echo first-success"),
        script("echo second-denial; false"),
        answer(),
    ]);
    SessionEngine::new(&model, &runtime)
        .run(
            SessionBootstrap::new("first job", limits(), "fixture").with_final_state(&final_state),
            &mut history,
        )
        .unwrap();
    let saved = final_state
        .take()
        .expect("the session published its final state");
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

/// One mutation encodes the model-facing batches and nothing else.
///
/// `update` runs five to eight times per tool call, under the live lock. Exactly one field needs
/// an encoding to bound it — `record.groups`, whose ceiling is the model's context window — and
/// every other bound is a length or a counter. A mutation that walked the whole document, as it
/// did while a store ceiling was measured in JSON bytes, charged a busy session the user text and
/// the entire execution ledger on every step.
///
/// Driven at both sizes on purpose: a state whose corpus lives outside the groups is exactly the
/// one a document-wide measurement would still make look cheap.
#[test]
fn one_mutation_encodes_only_the_model_facing_batches() {
    for large in [false, true] {
        let mut stored = snapshot();
        if large {
            // A long-running session's corpus: model-facing batches just under the group ceiling
            // so the trimming loop never runs, plus user text and an execution ledger that dwarf
            // them and that nothing here has any reason to walk.
            stored.record.groups = (0..118)
                .map(|call| crate::history::ToolGroup {
                    call,
                    calls: script("echo one").tool_calls,
                    results: vec![crate::history::ToolResult {
                        id: "call-a".into(),
                        result: Excerpt::new(&"g".repeat(MAX_EXCERPT_BYTES), MAX_EXCERPT_BYTES),
                    }],
                    omitted: false,
                    provenance: None,
                })
                .collect();
            stored.record.user = "u".repeat(120 * 1024);
            stored.record.executions = (0..96)
                .map(|i| ExecutionRecord {
                    job: stored.record.job.clone(),
                    call: 1,
                    tool: format!("call-{i}"),
                    sequence: i + 1,
                    capability: "test.read".into(),
                    provenance: ExecutionProvenance::DirectReadOnly,
                    invocation: None,
                    evidence: vec![],
                    outcome: ExecutionOutcome::Succeeded,
                    result: Some(Excerpt::new(&"e".repeat(4096), MAX_EXCERPT_BYTES)),
                })
                .collect();
        }
        let groups = crate::checkpoint::encoded_len(&stored.record.groups).expect("groups encode");
        let rest = crate::checkpoint::encoded_len(&stored.record.user).expect("user text encodes")
            + crate::checkpoint::encoded_len(&stored.record.executions).expect("ledger encodes");
        assert_eq!(
            large,
            groups > crate::context::MAX_GROUP_BYTES / 2
                && groups <= crate::context::MAX_GROUP_BYTES,
            "the large pass parks its batches under the ceiling that would trim them: \
             {groups} bytes of groups"
        );
        let journal = ExecutionJournal::new(stored, None).expect("journal opens");

        for mutations in 1..4_usize {
            ENCODED_BYTES.with(|total| total.set(0));
            for _ in 0..mutations {
                journal
                    .update(|c| c.context_revision += 1)
                    .expect("the mutation is applied");
            }
            let encoded = ENCODED_BYTES.with(std::cell::Cell::get);
            assert!(
                encoded <= mutations * (groups + 64),
                "{mutations} mutations measure the {groups} bytes of batches {mutations} times \
                 and not the {rest} bytes beside them: {encoded} bytes measured"
            );
        }
    }
}

/// Trimming an oversized group set marks what it omitted, and stops as soon as the rest fits.
///
/// The group ceiling is enforced on every mutation, but nothing pinned what enforcement does:
/// `update`'s loop is the only place a live session's model-facing batches are dropped, and an
/// omitted batch that lost its `omitted` marker would orphan its results in the ledger.
#[test]
fn a_mutation_over_the_group_ceiling_omits_batches_until_the_rest_fits() {
    let journal = ExecutionJournal::new(snapshot(), None).expect("journal opens");
    journal
        .update(|c| {
            c.record.groups = (0..4)
                .map(|call| crate::history::ToolGroup {
                    call,
                    calls: script("echo one").tool_calls,
                    results: vec![crate::history::ToolResult {
                        id: "call-a".into(),
                        result: Excerpt::new(&"r".repeat(200 * 1024), 256 * 1024),
                    }],
                    omitted: false,
                    provenance: None,
                })
                .collect();
        })
        .expect("the mutation is applied");

    let groups = &journal.snapshot().record.groups;
    assert_eq!(
        groups.len(),
        4,
        "an omitted batch keeps its position marker"
    );
    let omitted = groups.iter().filter(|g| g.omitted).count();
    assert!(
        (1..4).contains(&omitted),
        "it omits only as many as the ceiling needs: {omitted} of 4"
    );
    for group in groups.iter().filter(|g| g.omitted) {
        assert!(
            group.calls.is_empty() && group.results.is_empty(),
            "an omitted batch carries no calls or results"
        );
    }
    assert!(
        crate::checkpoint::encoded_len(groups).expect("the groups encode")
            <= crate::context::MAX_GROUP_BYTES,
        "the retained groups are inside the model-facing ceiling"
    );
}
