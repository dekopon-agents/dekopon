//! The model/tool transcript is opt-in, and lands on the log stream rather than on spans.
//!
//! # Why this is its own test binary
//!
//! Same reason as `prompt_tracing.rs`: `tracing` caches per-callsite interest globally the first
//! time a callsite is hit, so a callsite first reached with no subscriber installed stays disabled
//! for every later thread-local subscriber. Sharing a binary with other tests that call
//! `run_prompt` would make these assertions depend on execution order.

use std::sync::{Arc, Mutex};

use dekopon_model::model::{
    AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool, ModelToolCall,
};
use dekopon_run::prompt::{PromptLimits, SCRIPT_TOOL_NAME, ScriptRuntime, run_prompt};
use dekopon_shell::{Interpreter, Limits, ScriptOutcome};
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt as _;

const PROMPT_SENTINEL: &str = "SENTINEL_PROMPT_TEXT";
const SCRIPT_SENTINEL: &str = "SENTINEL_SCRIPT_TOKEN";
const ANSWER_SENTINEL: &str = "SENTINEL_ANSWER_TEXT";

/// Captures every log event's rendered fields.
struct EventLayer {
    events: Arc<Mutex<String>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut sink = self.events.lock().expect("event sink");
        event.record(&mut Visitor(&mut sink));
        sink.push('\n');
    }
}

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push_str(&format!(" {}={value}", field.name()));
    }
}

/// A model that requests one script and then answers with recognizable text.
struct ScriptedModel;

impl ChatModel for ScriptedModel {
    fn complete(
        &self,
        messages: &[ModelMessage],
        _tools: &[ModelTool],
    ) -> Result<AssistantTurn, ModelError> {
        if messages.iter().any(|message| message.role() == "tool") {
            return Ok(AssistantTurn {
                content: Some(ANSWER_SENTINEL.to_owned()),
                tool_calls: Vec::new(),
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
                    arguments: json!({ "script": format!("echo {SCRIPT_SENTINEL}") }).to_string(),
                },
            }],
            replay_items: Vec::new(),
        })
    }
}

struct ShellRuntime;

impl ScriptRuntime for ShellRuntime {
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
    ) -> dekopon_shell::CapabilityCallResult {
        dekopon_shell::CapabilityCallResult::NotFound
    }
}

/// Runs one session with payloads set as given and returns everything the log stream saw.
fn session_events(payloads: bool) -> String {
    let events = Arc::new(Mutex::new(String::new()));
    let recorded = Arc::clone(&events);
    let subscriber = tracing_subscriber::registry().with(EventLayer { events: recorded });
    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        dekopon_core::set_telemetry_payloads(payloads);
        run_prompt(
            &ScriptedModel,
            &ShellRuntime,
            PROMPT_SENTINEL,
            None,
            PromptLimits {
                max_steps: 4,
                max_capability_calls: 8,
            },
        )
        .expect("prompt session succeeds");
        dekopon_core::set_telemetry_payloads(false);
    });
    events.lock().expect("event sink").clone()
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

    // The metadata events still fire, so a failure below is about content rather than about the
    // session having failed to run at all.
    assert!(quiet.contains("agent.model.requested"), "{quiet}");
    assert!(quiet.contains("agent.tool.invocation.started"), "{quiet}");

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
}
