//! Token usage lands on `accounting.model.call` as span attributes.
//!
//! # Why this is its own test binary
//!
//! Same reason as `prompt_tracing.rs`: `tracing` caches per-callsite interest globally the first
//! time a callsite is hit, so a callsite first reached with no subscriber installed stays disabled
//! for every later thread-local subscriber. Sharing a binary with other tests that call
//! `run_prompt` would make this assertion depend on execution order.

use dekopon_harness::{
    bootstrap::{BootstrapError, CapabilitySnapshot, SessionBootstrap},
    history::History,
    runtime::ScriptRuntime,
    session::{PromptLimits, SessionEngine},
};
use dekopon_model::model::{
    AssistantTurn, ChatModel, ModelError, ModelMessage, ModelTool, ModelUsage,
};
use dekopon_shell::ScriptOutcome;
use dekopon_test_support::CaptureLayer;
use tracing_subscriber::layer::SubscriberExt as _;

/// A model that answers immediately, reporting usage for its single billed call.
struct AccountedModel;

impl ChatModel for AccountedModel {
    fn complete(
        &self,
        _messages: &[ModelMessage],
        _tools: &[ModelTool],
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
        let result: Result<AssistantTurn, ModelError> = {
            Ok(AssistantTurn {
                content: Some("done".to_owned()),
                tool_calls: Vec::new(),
                usage: Some(ModelUsage {
                    input_tokens: Some(41),
                    cached_input_tokens: Some(17),
                    output_tokens: Some(5),
                    reasoning_output_tokens: Some(3),
                    total_tokens: Some(46),
                }),
                replay_items: Vec::new(),
            })
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

struct NoScripts;

impl ScriptRuntime for NoScripts {
    fn capability_snapshot(&self) -> Result<CapabilitySnapshot, BootstrapError> {
        Ok(CapabilitySnapshot::empty())
    }
    fn run_script(&self, _script: &str, _max_capability_calls: u32) -> ScriptOutcome {
        unreachable!("the model answers without requesting a script")
    }
}

#[test]
fn usage_is_recorded_on_the_model_turn_span() {
    let captured = CaptureLayer::new();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        SessionEngine::new(&AccountedModel, &NoScripts)
            .run(
                SessionBootstrap::new(
                    "account for it",
                    PromptLimits {
                        max_steps: 1,
                        max_capability_calls: 1,
                    },
                    "fixture-model",
                ),
                &mut History::default(),
            )
            .expect("prompt session succeeds");
    });

    let fields = captured.spans_text();
    for usage in [
        "accounting.model.call usage.input_tokens=41",
        "accounting.model.call usage.cached_input_tokens=17",
        "accounting.model.call usage.output_tokens=5",
        "accounting.model.call usage.reasoning_output_tokens=3",
        "accounting.model.call usage.total_tokens=46",
    ] {
        assert!(fields.contains(usage), "{usage} missing: {fields}");
    }
}
