//! Token usage lands on `prompt.model_turn` as span attributes.
//!
//! # Why this is its own test binary
//!
//! Same reason as `prompt_tracing.rs`: `tracing` caches per-callsite interest globally the first
//! time a callsite is hit, so a callsite first reached with no subscriber installed stays disabled
//! for every later thread-local subscriber. Sharing a binary with other tests that call
//! `run_prompt` would make this assertion depend on execution order.

use std::sync::{Arc, Mutex};

use dekopon_model::model::{
    AssistantTurn, ChatModel, ModelError, ModelMessage, ModelTool, ModelUsage,
};
use dekopon_run::prompt::{PromptLimits, ScriptRuntime, run_prompt};
use dekopon_shell::ScriptOutcome;
use tracing_subscriber::layer::SubscriberExt as _;

/// Renders every field recorded on a live span as `name=value`, prefixed with the span's name.
struct SpanFieldLayer {
    fields: Arc<Mutex<String>>,
}

impl<S> tracing_subscriber::Layer<S> for SpanFieldLayer
where
    S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut sink = self.fields.lock().expect("field sink");
        sink.push_str(span.name());
        values.record(&mut Visitor(&mut sink));
        sink.push('\n');
    }
}

struct Visitor<'a>(&'a mut String);

impl tracing::field::Visit for Visitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

/// A model that answers immediately, reporting usage for its single billed call.
struct AccountedModel;

impl ChatModel for AccountedModel {
    fn complete(
        &self,
        _messages: &[ModelMessage],
        _tools: &[ModelTool],
    ) -> Result<AssistantTurn, ModelError> {
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
    }
}

struct NoScripts;

impl ScriptRuntime for NoScripts {
    fn run_script(&self, _script: &str, _max_capability_calls: u32) -> ScriptOutcome {
        unreachable!("the model answers without requesting a script")
    }
}

#[test]
fn usage_is_recorded_on_the_model_turn_span() {
    let fields = Arc::new(Mutex::new(String::new()));
    let recorded = Arc::clone(&fields);
    let subscriber = tracing_subscriber::registry().with(SpanFieldLayer { fields: recorded });
    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        run_prompt(
            &AccountedModel,
            &NoScripts,
            "account for it",
            None,
            PromptLimits {
                max_steps: 1,
                max_capability_calls: 1,
            },
        )
        .expect("prompt session succeeds");
    });

    let fields = fields.lock().expect("field sink");
    for usage in [
        "prompt.model_turn usage.input_tokens=41",
        "prompt.model_turn usage.cached_input_tokens=17",
        "prompt.model_turn usage.output_tokens=5",
        "prompt.model_turn usage.reasoning_output_tokens=3",
        "prompt.model_turn usage.total_tokens=46",
    ] {
        assert!(fields.contains(usage), "{usage} missing: {fields}");
    }
}
