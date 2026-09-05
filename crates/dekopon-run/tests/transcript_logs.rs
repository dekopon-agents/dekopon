//! The model/tool transcript is opt-in, and lands on the log stream rather than on spans.
//!
//! # Why this is its own test binary
//!
//! Same reason as `prompt_tracing.rs`: `tracing` caches per-callsite interest globally the first
//! time a callsite is hit, so a callsite first reached with no subscriber installed stays disabled
//! for every later thread-local subscriber. Sharing a binary with other tests that call
//! `run_prompt` would make these assertions depend on execution order.

use dekopon_harness::tools::SCRIPT_TOOL_NAME;
use dekopon_harness::{
    bootstrap::{BootstrapError, CapabilitySnapshot, SessionBootstrap},
    history::History,
    runtime::ScriptRuntime,
    session::{PromptLimits, SessionEngine},
};
use dekopon_model::model::{
    AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool,
    ModelToolCall, ModelUsage,
};
use dekopon_shell::{Interpreter, Limits, ScriptOutcome};
use dekopon_test_support::CaptureLayer;
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt as _;

const PROMPT_SENTINEL: &str = "SENTINEL_PROMPT_TEXT";
const SCRIPT_SENTINEL: &str = "SENTINEL_SCRIPT_TOKEN";
const ANSWER_SENTINEL: &str = "SENTINEL_ANSWER_TEXT";

/// A model that requests one script and then answers with recognizable text.
struct ScriptedModel;

impl ChatModel for ScriptedModel {
    fn complete(
        &self,
        messages: &[ModelMessage],
        _tools: &[ModelTool],
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
        #[allow(
            clippy::redundant_closure_call,
            reason = "fixture early returns must still record usage before propagation"
        )]
        let result: Result<AssistantTurn, ModelError> = (|| {
            if messages.iter().any(|message| message.role() == "tool") {
                return Ok(AssistantTurn {
                    content: Some(ANSWER_SENTINEL.to_owned()),
                    tool_calls: Vec::new(),
                    usage: Some(ModelUsage {
                        input_tokens: Some(23),
                        cached_input_tokens: Some(9),
                        output_tokens: Some(4),
                        reasoning_output_tokens: Some(2),
                        total_tokens: Some(27),
                    }),
                    replay_items: Vec::new(),
                });
            }
            Ok(AssistantTurn {
                content: None,
                tool_calls: vec![ModelToolCall {
                    id: "call-1".to_owned(),
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: SCRIPT_TOOL_NAME.to_owned(),
                        arguments: json!({ "script": format!("echo {SCRIPT_SENTINEL}") })
                            .to_string(),
                    },
                }],
                usage: Some(ModelUsage {
                    input_tokens: Some(11),
                    cached_input_tokens: None,
                    output_tokens: Some(3),
                    reasoning_output_tokens: None,
                    total_tokens: Some(14),
                }),
                replay_items: Vec::new(),
            })
        })();
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

struct ShellRuntime;

impl ScriptRuntime for ShellRuntime {
    fn capability_snapshot(&self) -> Result<CapabilitySnapshot, BootstrapError> {
        Ok(CapabilitySnapshot::empty())
    }
    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
        Interpreter::new(Limits {
            max_capability_calls,
            ..Limits::default()
        })
        .run(script, &NoCapabilities)
    }
}

struct NoCapabilities;

impl dekopon_shell::CapabilityInvoker for NoCapabilities {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }

    fn invoke(
        &self,
        _capability: &str,
        _input: serde_json::Value,
        _secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> dekopon_shell::CapabilityCallResult {
        dekopon_shell::CapabilityCallResult::NotFound
    }
}

/// Runs one session with payloads set as given and returns everything the log stream saw.
fn session_events(payloads: bool) -> String {
    let captured = CaptureLayer::new();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        dekopon_core::set_telemetry_payloads(payloads);
        SessionEngine::new(&ScriptedModel, &ShellRuntime)
            .run(
                SessionBootstrap::new(
                    PROMPT_SENTINEL,
                    PromptLimits {
                        max_steps: 4,
                        max_capability_calls: 8,
                    },
                    "fixture-model",
                ),
                &mut History::default(),
            )
            .expect("prompt session succeeds");
        dekopon_core::set_telemetry_payloads(false);
    });
    captured.events_text()
}

/// Quiet then verbose, in one test on purpose.
///
/// `telemetry_payloads` is process-wide state, so two tests toggling it in the same binary race:
/// the verbose case can flip the flag while the quiet case is mid-session, and the quiet
/// assertions then fail for a reason that has nothing to do with the code under test. One
/// sequential test is the honest structure for a process-wide switch.
#[test]
fn transcript_is_opt_in_and_carries_the_whole_exchange() {
    const TRANSCRIPT_EVENTS: [&str; 4] = [
        "agent.model.prompt",
        "agent.model.answer",
        "agent.tool.script",
        "agent.tool.output",
    ];
    const SENTINELS: [&str; 3] = [PROMPT_SENTINEL, SCRIPT_SENTINEL, ANSWER_SENTINEL];

    let quiet = session_events(false);

    // The accounting record still fires in either mode, so a failure below is about content
    // rather than about the session having failed to run at all.
    assert!(quiet.contains("accounting.model.call"), "{quiet}");

    // Token usage is accounting, not payload: the counts ride the accounting record even in quiet
    // mode, one set per turn, exactly as the model reported them.
    for usage in [
        "usage.input_tokens=11",
        "usage.total_tokens=14",
        "usage.input_tokens=23",
        "usage.cached_input_tokens=9",
        "usage.reasoning_output_tokens=2",
        "usage.total_tokens=27",
    ] {
        assert!(quiet.contains(usage), "{usage} missing: {quiet}");
    }

    // The span-mirroring lifecycle events are gone: a span already carries start, end, duration,
    // and parent, and a log pair repeating that is volume without information.
    for mirrored in [
        "agent.model.requested",
        "agent.model.completed",
        "agent.tool.invocation.started",
        "agent.tool.invocation.completed",
        "agent.session.started",
        "shell.command.started",
        "shell.command.completed",
    ] {
        assert!(
            !quiet.contains(mirrored),
            "{mirrored} still emitted; it mirrors a span: {quiet}"
        );
    }

    for event in TRANSCRIPT_EVENTS {
        assert!(
            !quiet.contains(event),
            "{event} emitted while quiet: {quiet}"
        );
    }
    for sentinel in SENTINELS {
        assert!(
            !quiet.contains(sentinel),
            "{sentinel} leaked while quiet: {quiet}"
        );
    }

    let verbose = session_events(true);

    for event in TRANSCRIPT_EVENTS {
        assert!(verbose.contains(event), "{event} missing: {verbose}");
    }
    for sentinel in SENTINELS {
        assert!(
            verbose.contains(sentinel),
            "{sentinel} missing from transcript: {verbose}"
        );
    }

    // The transcript is shipped once and then extended, never re-shipped whole. Turn N's message
    // vector strictly contains turn N-1's, so logging all of it every turn costs a session O(N^2)
    // payload bytes to repeat what `agent.model.answer`, `agent.tool.script`, and
    // `agent.tool.output` already said on the turn that produced it.
    let prompts = verbose
        .lines()
        .filter(|line| line.contains("agent.model.prompt"))
        .collect::<Vec<_>>();

    assert_eq!(prompts.len(), 2, "one prompt event per turn: {verbose}");
    assert!(
        prompts[0].contains("transcript.scope=\"full\""),
        "{}",
        prompts[0]
    );
    assert!(prompts[0].contains(PROMPT_SENTINEL), "{}", prompts[0]);

    assert!(
        prompts[1].contains("transcript.scope=\"delta\""),
        "{}",
        prompts[1]
    );
    // The second request still carries the whole conversation — `message.count` says how much of
    // it there is — and the event carries only what this turn appended.
    assert!(prompts[1].contains("message.count=4"), "{}", prompts[1]);
    assert!(
        !prompts[1].contains(PROMPT_SENTINEL),
        "the prompt was re-shipped: {}",
        prompts[1]
    );
    assert!(
        prompts[1].contains(SCRIPT_SENTINEL),
        "the appended tool traffic is missing: {}",
        prompts[1]
    );
}
