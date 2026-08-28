//! Interpreter spans must nest under the script span that drove them, across the blocking bridge.
//!
//! # Why this is its own test binary
//!
//! `tracing` caches per-callsite interest **globally and once**, the first time a callsite is hit.
//! A callsite first reached while no subscriber is installed caches as `Interest::never()` and
//! stays disabled for every later thread-local subscriber. Every other test that calls
//! `run_prompt` does exactly that, so sharing a binary with them made this assertion depend on
//! test execution order — it passed roughly six runs in eight and failed the rest, on CI and
//! locally alike.
//!
//! One test per binary removes the race rather than papering over it: nothing else can reach
//! `prompt.session` or `prompt.script` before the capturing subscriber is in place.

use std::sync::{Arc, Mutex};

use dekopon_model::model::{
    AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool, ModelToolCall,
};
use dekopon_run::prompt::{PromptLimits, SCRIPT_TOOL_NAME, ScriptRuntime, run_prompt};
use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, Interpreter, Limits, ScriptOutcome};
use serde_json::{Value, json};
use tracing_subscriber::layer::SubscriberExt;

/// One closed span: its name, and its enclosing span names innermost-first.
type SpanAncestry = (String, Vec<String>);

/// Records the ancestry of every span, so the test can assert what nests inside what.
struct SpanTreeLayer {
    spans: Arc<Mutex<Vec<SpanAncestry>>>,
}

impl<S> tracing_subscriber::Layer<S> for SpanTreeLayer
where
    S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    fn on_close(&self, id: tracing::span::Id, context: tracing_subscriber::layer::Context<'_, S>) {
        let Some(span) = context.span(&id) else {
            return;
        };
        self.spans.lock().expect("span lock").push((
            span.name().to_owned(),
            span.scope()
                .skip(1)
                .map(|parent| parent.name().to_owned())
                .collect(),
        ));
    }
}

/// A model that requests one script and then answers.
struct ScriptedModel {
    script: String,
}

impl ChatModel for ScriptedModel {
    fn complete(
        &self,
        messages: &[ModelMessage],
        _tools: &[ModelTool],
    ) -> Result<AssistantTurn, ModelError> {
        if messages.iter().any(|message| message.role() == "tool") {
            return Ok(AssistantTurn {
                content: Some("done".to_owned()),
                tool_calls: Vec::new(),
                usage: None,
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
                    arguments: json!({ "script": self.script }).to_string(),
                },
            }],
            usage: None,
            replay_items: Vec::new(),
        })
    }
}

/// An invoker that is genuinely asynchronous underneath, as the broker leg is.
struct BridgedInvoker {
    handle: tokio::runtime::Handle,
}

impl CapabilityInvoker for BridgedInvoker {
    fn granted(&self) -> Vec<String> {
        vec!["echo.echo".to_owned()]
    }

    fn invoke(
        &self,
        _capability: &str,
        input: Value,
        _secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        // A real await point, reached from the same blocking-pool thread the session runs on.
        self.handle.block_on(async move {
            tokio::task::yield_now().await;
            CapabilityCallResult::Succeeded(input)
        })
    }
}

/// A runtime that runs the real interpreter, exactly as `dekopon-run` does in production.
struct BridgedShellRuntime {
    handle: tokio::runtime::Handle,
}

impl ScriptRuntime for BridgedShellRuntime {
    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
        Interpreter::new(Limits {
            max_capability_calls,
            ..Limits::default()
        })
        .run(
            script,
            &BridgedInvoker {
                handle: self.handle.clone(),
            },
        )
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn interpreter_spans_nest_under_the_script_span_across_the_blocking_bridge() {
    // The interpreter creates its spans with no propagation code at all, relying on the whole
    // session — including each broker round trip, bridged from this same blocking-pool thread —
    // staying on one thread. That is an assumption about the runtime rather than about `tracing`,
    // so it is checked against a real `spawn_blocking` and a real `Handle::block_on`.
    let spans = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&spans);
    let handle = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || {
        let subscriber = tracing_subscriber::registry().with(SpanTreeLayer { spans: recorded });
        tracing::subscriber::with_default(subscriber, || {
            // Belt and braces alongside the one-test-per-binary isolation above: any callsite
            // already cached as uninteresting re-registers against the subscriber just installed.
            tracing::callsite::rebuild_interest_cache();
            let session = tracing::info_span!("runner.prompt");
            let _entered = session.enter();
            let model = ScriptedModel {
                script: "echo one\necho.echo --message two".to_owned(),
            };
            let runtime = BridgedShellRuntime { handle };
            run_prompt(
                &model,
                &runtime,
                "trace it",
                None,
                PromptLimits {
                    max_steps: 4,
                    max_capability_calls: 32,
                },
            )
            .expect("prompt session succeeds");
        });
    })
    .await
    .expect("blocking prompt task completes");

    let spans = spans.lock().expect("span lock");
    let commands = spans
        .iter()
        .filter(|(name, _)| name == "shell.command")
        .collect::<Vec<_>>();
    assert_eq!(commands.len(), 2, "one span per command the script ran");
    for (_, parents) in commands {
        // `prompt.model_turn` is absent by design: the loop drops that guard once the model has
        // answered, so a script runs under the session rather than under the turn. The
        // interpreter's own `shell.script` sits innermost, where it holds the run's totals.
        assert_eq!(
            parents,
            &[
                "shell.script".to_owned(),
                "prompt.script".to_owned(),
                "prompt.session".to_owned(),
                "runner.prompt".to_owned(),
            ],
            "an interpreter span must land inside the script span that drove it"
        );
    }
}
