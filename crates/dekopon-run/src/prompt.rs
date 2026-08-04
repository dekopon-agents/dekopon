//! The model tool loop, exposing one sandboxed scripting tool rather than one tool per capability.
//!
//! Phase 1 built the interpreter; this module is the model-facing half. A session offers exactly
//! one tool, [`SCRIPT_TOOL_NAME`], whose single argument is a script. Everything a model wants to
//! do — inspect what it can reach, loop, branch, parse JSON, call capabilities — happens inside
//! that script instead of across many small tool calls.

use dekopon_model::model::{ChatModel, ModelError, ModelMessage, ModelTool, assistant_message};
use dekopon_shell::ScriptOutcome;
use serde_json::{Value, json};
use thiserror::Error;

/// Model-facing name of the single scripting tool.
///
/// Named for what it resembles rather than what it is. Models have overwhelming priors about a
/// tool called `bash`, and almost all of them transfer: pipelines, `&&`, `$( )`, exit codes. The
/// description below spends its length on the places where those priors are wrong.
pub const SCRIPT_TOOL_NAME: &str = "bash";

/// Tool calls a single model turn may request.
///
/// This bound used to cover one capability invocation each, so 32 was a statement about how much
/// provider work one turn could drive. With one scripting tool it no longer is: a single script
/// can drive many invocations, so the real work bound moved to
/// [`PromptLimits::max_capability_calls`], which the interpreter enforces per script and this loop
/// enforces across the session.
///
/// What is left is a well-formedness bound. A scripting tool expresses a multi-step plan *inside*
/// one script, so a correct turn calls it once; a handful of calls is tolerated for models that
/// split work across parallel calls, and anything beyond that is a runaway rather than a plan.
const MAX_SCRIPT_CALLS_PER_TURN: usize = 4;

/// Script execution boundary consumed by the prompt loop.
///
/// This deliberately returns no `Result`. A script failure — a parse error, an exhausted budget, a
/// capability that policy refused — is a script *outcome*, and the model reads it and recovers the
/// same way it would from a non-zero exit code in a terminal. Only a broken session aborts the
/// loop.
pub trait ScriptRuntime {
    /// Runs one model-authored script, invoking at most `max_capability_calls` capabilities.
    ///
    /// The ceiling is supplied per call rather than fixed at construction because the prompt loop
    /// spends one session-wide budget across every script it runs.
    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome;
}

/// Bounds on one prompt session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptLimits {
    /// Maximum model turns, including the turn that produces the final answer.
    pub max_steps: u32,
    /// Capability invocations the whole session may drive, summed across every script.
    pub max_capability_calls: u32,
}

/// Result of a completed prompt/tool session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptOutcome {
    /// Final assistant text.
    pub answer: String,
    /// Number of model requests made.
    pub model_turns: u32,
    /// Number of scripts the model ran.
    pub script_calls: u32,
    /// Capability invocations those scripts drove.
    pub capability_invocations: u32,
}

/// Runs a bounded prompt/tool loop over one scripting tool.
///
/// This is synchronous on purpose. Both boundaries it sits between — `ChatModel` and
/// [`ScriptRuntime`] — are synchronous by design, so the caller runs the whole loop on a blocking
/// task rather than colouring these signatures `async`.
pub fn run_prompt<M, R>(
    model: &M,
    runtime: &R,
    prompt: &str,
    system: Option<&str>,
    limits: PromptLimits,
) -> Result<PromptOutcome, PromptError>
where
    M: ChatModel + ?Sized,
    R: ScriptRuntime + ?Sized,
{
    if limits.max_steps == 0 {
        return Err(PromptError::ZeroSteps);
    }

    let model_tools = vec![script_tool()];
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.push(ModelMessage::system(system));
    }
    messages.push(ModelMessage::user(prompt));

    let session_span = tracing::info_span!(
        "prompt.session",
        prompt.max_steps = limits.max_steps,
        prompt.max_capability_calls = limits.max_capability_calls
    );
    let _session = session_span.enter();
    let mut script_calls = 0_u32;
    let mut capability_invocations = 0_u32;

    for model_turns in 1..=limits.max_steps {
        let turn = model.complete(&messages, &model_tools)?;
        messages.push(assistant_message(&turn));

        if turn.tool_calls.is_empty() {
            let answer = turn
                .content
                .filter(|content| !content.trim().is_empty())
                .ok_or(PromptError::EmptyAnswer)?;
            return Ok(PromptOutcome {
                answer,
                model_turns,
                script_calls,
                capability_invocations,
            });
        }
        if turn.tool_calls.len() > MAX_SCRIPT_CALLS_PER_TURN {
            return Err(PromptError::TooManyToolCalls {
                actual: turn.tool_calls.len(),
                maximum: MAX_SCRIPT_CALLS_PER_TURN,
            });
        }

        for call in turn.tool_calls {
            if call.id.trim().is_empty() {
                return Err(PromptError::EmptyToolCallId);
            }
            if call.function.name != SCRIPT_TOOL_NAME {
                return Err(PromptError::UnknownTool(call.function.name));
            }
            let script = script_argument(&call.function.name, &call.function.arguments)?;

            // Whatever the session has already spent is unavailable to this script, so a model
            // cannot widen its own budget by splitting work across more tool calls.
            let remaining = limits
                .max_capability_calls
                .saturating_sub(capability_invocations);
            let span = tracing::info_span!(
                "prompt.script",
                script.max_capability_calls = remaining,
                script.bytes = script.len()
            );
            let outcome = {
                let _entered = span.enter();
                runtime.run_script(&script, remaining)
            };
            tracing::info!(
                script.exit_code = outcome.exit_code.get(),
                script.steps = outcome.steps,
                script.capability_calls = outcome.capability_calls,
                script.truncated = outcome.truncated,
                "model script completed"
            );
            script_calls = script_calls.saturating_add(1);
            capability_invocations =
                capability_invocations.saturating_add(outcome.capability_calls);
            messages.push(ModelMessage::tool(call.id, format_script_outcome(&outcome)));
        }
    }

    Err(PromptError::MaxSteps {
        maximum: limits.max_steps,
    })
}

/// Renders one script outcome the way a terminal would: output, then an exit-code trailer.
///
/// `dekopon-run shell` prints this exact shape to a human and the prompt loop hands this exact
/// shape to a model, so a script a model wrote behaves identically when an operator reruns it.
#[must_use]
pub fn format_script_outcome(outcome: &ScriptOutcome) -> String {
    let mut text = outcome.output.clone();
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&format!("[exit code: {}]", outcome.exit_code));
    text
}

/// Builds the one tool a prompt session offers.
fn script_tool() -> ModelTool {
    ModelTool {
        name: SCRIPT_TOOL_NAME.to_owned(),
        description: SCRIPT_TOOL_DESCRIPTION.to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The script to run. Multiple lines are expected and encouraged."
                }
            },
            "required": ["script"],
            "additionalProperties": false
        }),
    }
}

/// Extracts the `script` argument from one model tool call.
fn script_argument(tool: &str, arguments: &str) -> Result<String, PromptError> {
    let arguments = serde_json::from_str::<Value>(arguments).map_err(|source| {
        PromptError::InvalidArguments {
            tool: tool.to_owned(),
            source,
        }
    })?;
    let Value::Object(arguments) = arguments else {
        return Err(PromptError::ArgumentsNotObject {
            tool: tool.to_owned(),
        });
    };
    match arguments.get("script") {
        Some(Value::String(script)) => Ok(script.clone()),
        _ => Err(PromptError::MissingScript {
            tool: tool.to_owned(),
        }),
    }
}

/// The whole model-facing surface of a Dekopon session.
///
/// This replaces one JSON Schema per capability, so it is allowed to be long: it is paid once per
/// request instead of once per capability, and it shrinks rather than grows as an operator grants
/// more. What it must *not* do is describe anything the interpreter does not have. There is no
/// `help` builtin — the runtime discovery surface is `cap --list` and `cap --describe`, and
/// pointing a model at anything else would spend a tool call on "command not found".
const SCRIPT_TOOL_DESCRIPTION: &str = "\
Run one script in Dekopon's sandboxed shell. Returns the script's combined output followed by an \
`[exit code: N]` trailer, exactly as a terminal would.

The dialect is eerily close to bash and explicitly not bash. Pipelines, `&&`, `||`, `;`, a leading \
`!`, `if`/`elif`/`else`, `for`, `while`, `until`, `break`/`continue`, functions with `$1`/`$@`/\
`$#`/`shift`/`local`, `$NAME`, `${NAME[index]}`, `$( )`, `$(( ))`, `$?`, `return`, `exit`, both \
quoting forms, and `>`/`>>` into named in-memory buffers all behave the way you expect. \
Everything outside that curated set fails loudly and by name: `eval`, backticks, subshells, \
`[[ ]]`, `case`, `set -e`, `2>&1`, here-documents, and `&` backgrounding are errors, never silent \
no-ops. If a script ran, it did what it said.

Four things genuinely differ from a real shell:

1. Commands are Dekopon capabilities, not programs. A command word containing `.`, `-`, or `_` is \
a capability invocation; every other word is a builtin. There are no processes, no filesystem, no \
environment variables, and no network reachable except through a capability.
2. Capability arguments are `--kebab-case` flags that become one JSON object: \
`posts.get --post-id 7 --include-body` sends `{\"postId\": 7, \"includeBody\": true}`. A repeated \
flag becomes an array, and a single bare `{...}` argument is used as the input verbatim.
3. Values are JSON, not text. `|` hands a structured value to the next command, and `jq` is built \
in to work on it.
4. The session is bounded. Steps, output, wall-clock time, and capability calls all have ceilings; \
tripping one ends the script with a message naming it.

Builtins: `jq`, `curl`, `cap`, `cat`, `echo`, `printf`, `test`/`[`, `true`, `false`, `sleep`, \
`grep`, `sed`, `cut`, `sort`, `uniq`, `wc`, `base64`, `xargs`. `curl` opens no socket of its own — \
it assembles a request for whichever HTTP capability this session was given, and is \"command not \
found\" when it was given none. `grep` and `sed` patterns are literal text, not regular \
expressions; use `jq` for real matching.

There is no `help`. Discover this session with `cap --list`, which returns a JSON array of the \
capability IDs you may invoke, and `cap --describe <capability>`, which returns one capability's \
input schema. Then prefer a single script that does the whole job over many small ones — that is \
the entire point of this tool.";

/// Failure to complete a prompt/tool session.
///
/// Every variant here is a broken *session*, not a failed script. A script that parses badly,
/// trips a budget, or calls a capability policy refuses is reported to the model through
/// [`format_script_outcome`] so it can recover.
#[derive(Debug, Error)]
pub enum PromptError {
    /// A zero-length loop was requested.
    #[error("prompt max steps must be greater than zero")]
    ZeroSteps,
    /// A model request failed.
    #[error(transparent)]
    Model(#[from] ModelError),
    /// The model selected a tool that was not offered.
    #[error("model requested unknown tool {0:?}; this session offers only {SCRIPT_TOOL_NAME:?}")]
    UnknownTool(String),
    /// A model requested more scripts in one turn than a plan ever needs.
    #[error("model returned {actual} tool calls in one turn; the maximum is {maximum}")]
    TooManyToolCalls {
        /// Model-requested call count.
        actual: usize,
        /// Fixed per-turn bound.
        maximum: usize,
    },
    /// A model supplied an empty tool-call correlation ID.
    #[error("model returned an empty tool-call ID")]
    EmptyToolCallId,
    /// Tool arguments were malformed JSON.
    #[error("model returned invalid JSON arguments for tool {tool:?}")]
    InvalidArguments {
        /// Prompt-visible tool name.
        tool: String,
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Tool arguments were valid JSON but not an object.
    #[error("model arguments for tool {tool:?} must be a JSON object")]
    ArgumentsNotObject {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// Tool arguments carried no script to run.
    #[error("model arguments for tool {tool:?} must include a string \"script\" field")]
    MissingScript {
        /// Prompt-visible tool name.
        tool: String,
    },
    /// The model ended without text or a tool call.
    #[error("model returned neither tool calls nor a final answer")]
    EmptyAnswer,
    /// The model did not produce a final answer within the configured loop bound.
    #[error("model did not produce a final answer within {maximum} turns")]
    MaxSteps {
        /// Configured model-turn limit.
        maximum: u32,
    },
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use dekopon_model::model::{
        AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool,
        ModelToolCall,
    };
    use dekopon_shell::{ExitCode, ScriptOutcome};
    use serde_json::{Value, json};

    use super::{
        PromptError, PromptLimits, SCRIPT_TOOL_NAME, ScriptRuntime, format_script_outcome,
        run_prompt, script_tool,
    };

    /// A model whose turns are fixed in advance, recording what it was asked.
    ///
    /// `Mutex` rather than `RefCell`: the loop now runs on a blocking task, so every fixture it
    /// touches has to cross a thread boundary.
    struct ScriptedModel {
        turns: Mutex<VecDeque<AssistantTurn>>,
        observed_tools: Mutex<Vec<Vec<ModelTool>>>,
        observed_tool_messages: Mutex<Vec<String>>,
    }

    impl ScriptedModel {
        fn new(turns: impl IntoIterator<Item = AssistantTurn>) -> Self {
            Self {
                turns: Mutex::new(turns.into_iter().collect()),
                observed_tools: Mutex::new(Vec::new()),
                observed_tool_messages: Mutex::new(Vec::new()),
            }
        }
    }

    impl ChatModel for ScriptedModel {
        fn complete(
            &self,
            messages: &[ModelMessage],
            tools: &[ModelTool],
        ) -> Result<AssistantTurn, ModelError> {
            self.observed_tools
                .lock()
                .expect("tool observations lock")
                .push(tools.to_vec());
            self.observed_tool_messages
                .lock()
                .expect("tool message lock")
                .extend(
                    messages
                        .iter()
                        .filter(|message| message.role() == "tool")
                        .filter_map(|message| message.content().map(str::to_owned)),
                );
            self.turns
                .lock()
                .expect("turn lock")
                .pop_front()
                .ok_or(ModelError::NoChoices)
        }
    }

    /// A runtime that records the scripts and ceilings it was handed.
    struct RecordingRuntime {
        scripts: Mutex<Vec<(String, u32)>>,
        capability_calls_per_script: u32,
    }

    impl RecordingRuntime {
        fn new(capability_calls_per_script: u32) -> Self {
            Self {
                scripts: Mutex::new(Vec::new()),
                capability_calls_per_script,
            }
        }
    }

    impl ScriptRuntime for RecordingRuntime {
        fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
            self.scripts
                .lock()
                .expect("script lock")
                .push((script.to_owned(), max_capability_calls));
            let capability_calls = self.capability_calls_per_script.min(max_capability_calls);
            ScriptOutcome {
                output: format!("ran {} bytes", script.len()),
                exit_code: ExitCode::SUCCESS,
                truncated: false,
                capability_calls,
                steps: 1,
            }
        }
    }

    fn script_call(id: &str, script: &str) -> AssistantTurn {
        AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: id.to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": script }).to_string(),
                },
            }],
            replay_items: Vec::new(),
        }
    }

    fn answer(text: &str) -> AssistantTurn {
        AssistantTurn {
            content: Some(text.to_owned()),
            tool_calls: Vec::new(),
            replay_items: Vec::new(),
        }
    }

    fn limits(max_steps: u32, max_capability_calls: u32) -> PromptLimits {
        PromptLimits {
            max_steps,
            max_capability_calls,
        }
    }

    #[test]
    fn offers_exactly_one_scripting_tool() {
        let tool = script_tool();

        assert_eq!(tool.name, "bash");
        assert_eq!(tool.parameters["properties"]["script"]["type"], "string");
        assert_eq!(tool.parameters["required"], json!(["script"]));
        assert_eq!(tool.parameters["additionalProperties"], json!(false));
        // The description has to point at the interpreter's own self-disclosure, or a model has no
        // way to learn which capabilities this session can reach.
        assert!(tool.description.contains("cap --list"));
        assert!(tool.description.contains("cap --describe"));
        // ...and it must not invent a discovery command the interpreter does not implement. There
        // is no `help` builtin, so advertising one would spend a tool call on "command not found".
        assert!(tool.description.contains("There is no `help`"));
    }

    #[test]
    fn runs_a_model_script_and_returns_the_final_answer() {
        let model = ScriptedModel::new([
            script_call("call-1", "echo.echo --message hi | jq -r .message"),
            answer("The capability echoed hi."),
        ]);
        let runtime = RecordingRuntime::new(1);

        let outcome = run_prompt(&model, &runtime, "say hi", None, limits(4, 32))
            .expect("prompt session succeeds");

        assert_eq!(outcome.answer, "The capability echoed hi.");
        assert_eq!(outcome.model_turns, 2);
        assert_eq!(outcome.script_calls, 1);
        assert_eq!(outcome.capability_invocations, 1);
        let scripts = runtime.scripts.lock().expect("script lock");
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].0, "echo.echo --message hi | jq -r .message");
    }

    #[test]
    fn exposes_one_tool_per_request_regardless_of_capability_count() {
        let model = ScriptedModel::new([answer("done")]);
        let runtime = RecordingRuntime::new(0);

        run_prompt(&model, &runtime, "do nothing", None, limits(2, 32)).expect("prompt succeeds");

        let observed = model.observed_tools.lock().expect("tool observations lock");
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].len(), 1);
        assert_eq!(observed[0][0].name, SCRIPT_TOOL_NAME);
    }

    #[test]
    fn returns_script_output_and_exit_code_to_the_model() {
        let model = ScriptedModel::new([script_call("call-1", "echo hi"), answer("done")]);
        let runtime = RecordingRuntime::new(0);

        run_prompt(&model, &runtime, "run something", None, limits(4, 32))
            .expect("prompt session succeeds");

        let messages = model
            .observed_tool_messages
            .lock()
            .expect("tool message lock");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], "ran 7 bytes\n[exit code: 0]");
    }

    #[test]
    fn spends_one_capability_budget_across_every_script_in_the_session() {
        // The interpreter's own ceiling bounds one script. Without this, a model widens its budget
        // simply by writing more scripts, and `max_steps` multiplies rather than bounds the work.
        let model = ScriptedModel::new([
            script_call("call-1", "one"),
            script_call("call-2", "two"),
            script_call("call-3", "three"),
            answer("done"),
        ]);
        let runtime = RecordingRuntime::new(4);

        let outcome = run_prompt(&model, &runtime, "spend it", None, limits(8, 10))
            .expect("prompt session succeeds");

        let scripts = runtime.scripts.lock().expect("script lock");
        let ceilings = scripts
            .iter()
            .map(|(_, ceiling)| *ceiling)
            .collect::<Vec<_>>();
        assert_eq!(ceilings, vec![10, 6, 2]);
        assert_eq!(outcome.capability_invocations, 10);
    }

    #[test]
    fn exhausted_capability_budget_leaves_later_scripts_with_nothing_to_spend() {
        let model = ScriptedModel::new([
            script_call("call-1", "one"),
            script_call("call-2", "two"),
            answer("done"),
        ]);
        let runtime = RecordingRuntime::new(8);

        let outcome = run_prompt(&model, &runtime, "spend it", None, limits(8, 3))
            .expect("prompt session succeeds");

        let scripts = runtime.scripts.lock().expect("script lock");
        assert_eq!(scripts[1].1, 0);
        assert_eq!(outcome.capability_invocations, 3);
    }

    #[test]
    fn rejects_model_selected_tools_that_were_not_offered() {
        let model = ScriptedModel::new([AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: "call-1".to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: "echo_echo".to_owned(),
                    arguments: "{}".to_owned(),
                },
            }],
            replay_items: Vec::new(),
        }]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "call the old tool", None, limits(1, 32))
            .expect_err("unknown tools must fail closed");

        assert!(matches!(error, PromptError::UnknownTool(_)));
        assert!(runtime.scripts.lock().expect("script lock").is_empty());
    }

    #[test]
    fn rejects_tool_calls_without_a_string_script_argument() {
        for arguments in [r#"{"command":"echo hi"}"#, r#"{"script":42}"#, "{}"] {
            let model = ScriptedModel::new([AssistantTurn {
                content: None,
                tool_calls: vec![ModelToolCall {
                    id: "call-1".to_owned(),
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: SCRIPT_TOOL_NAME.to_owned(),
                        arguments: arguments.to_owned(),
                    },
                }],
                replay_items: Vec::new(),
            }]);
            let runtime = RecordingRuntime::new(0);

            let error = run_prompt(&model, &runtime, "malformed", None, limits(1, 32))
                .expect_err("a missing script must fail closed");

            assert!(
                matches!(error, PromptError::MissingScript { .. }),
                "{arguments}: {error}"
            );
            assert!(runtime.scripts.lock().expect("script lock").is_empty());
        }
    }

    #[test]
    fn bounds_script_fan_out_per_model_turn() {
        let tool_calls = (0..5)
            .map(|index| ModelToolCall {
                id: format!("call-{index}"),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": "echo hi" }).to_string(),
                },
            })
            .collect();
        let model = ScriptedModel::new([AssistantTurn {
            content: None,
            tool_calls,
            replay_items: Vec::new(),
        }]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "fan out", None, limits(1, 32))
            .expect_err("script fan-out must be bounded");

        assert!(matches!(error, PromptError::TooManyToolCalls { .. }));
        assert!(runtime.scripts.lock().expect("script lock").is_empty());
    }

    #[test]
    fn formats_an_empty_script_outcome_without_a_leading_blank_line() {
        let outcome = ScriptOutcome {
            output: String::new(),
            exit_code: ExitCode::NOT_FOUND,
            truncated: false,
            capability_calls: 0,
            steps: 1,
        };

        assert_eq!(format_script_outcome(&outcome), "[exit code: 127]");
    }

    /// A runtime whose capability dispatch is genuinely asynchronous underneath.
    ///
    /// This is the shape `dekopon-run` uses in production: a synchronous [`ScriptRuntime`] bridging
    /// to an async broker round trip with `Handle::block_on`, which is correct only because the
    /// whole loop runs on a blocking task rather than a runtime worker thread.
    struct BlockingBridgeRuntime {
        handle: tokio::runtime::Handle,
        dispatched: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptRuntime for BlockingBridgeRuntime {
        fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
            let dispatched = Arc::clone(&self.dispatched);
            let script = script.to_owned();
            let output = self.handle.block_on(async move {
                tokio::task::yield_now().await;
                dispatched
                    .lock()
                    .expect("dispatch lock")
                    .push(script.clone());
                format!("async runtime saw: {script}")
            });
            ScriptOutcome {
                output,
                exit_code: ExitCode::SUCCESS,
                truncated: false,
                capability_calls: 1.min(max_capability_calls),
                steps: 1,
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drives_the_loop_from_a_blocking_task_over_an_async_dispatch() {
        let dispatched = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::runtime::Handle::current();
        let recorded = Arc::clone(&dispatched);

        let outcome = tokio::task::spawn_blocking(move || {
            let model = ScriptedModel::new([
                script_call("call-1", "http.get --url https://example.test"),
                answer("fetched"),
            ]);
            let runtime = BlockingBridgeRuntime {
                handle,
                dispatched: recorded,
            };
            run_prompt(&model, &runtime, "fetch it", None, limits(4, 32))
        })
        .await
        .expect("blocking prompt task completes")
        .expect("prompt session succeeds");

        assert_eq!(outcome.answer, "fetched");
        assert_eq!(outcome.script_calls, 1);
        assert_eq!(outcome.capability_invocations, 1);
        assert_eq!(
            *dispatched.lock().expect("dispatch lock"),
            vec!["http.get --url https://example.test".to_owned()]
        );
    }

    #[test]
    fn rejects_a_zero_step_session_before_contacting_the_model() {
        let model = ScriptedModel::new([]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "nothing", None, limits(0, 32))
            .expect_err("a zero-step session is a usage error");

        assert!(matches!(error, PromptError::ZeroSteps));
        assert!(
            model
                .observed_tools
                .lock()
                .expect("tool observations lock")
                .is_empty()
        );
    }

    #[test]
    fn stops_when_the_model_never_produces_an_answer() {
        let model = ScriptedModel::new([
            script_call("call-1", "echo one"),
            script_call("call-2", "echo two"),
        ]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "loop forever", None, limits(2, 32))
            .expect_err("an answerless session must terminate");

        assert!(matches!(error, PromptError::MaxSteps { maximum: 2 }));
    }

    #[test]
    fn tool_call_ids_must_correlate() {
        let model = ScriptedModel::new([AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: "  ".to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": "echo hi" }).to_string(),
                },
            }],
            replay_items: Vec::new(),
        }]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "correlate", None, limits(1, 32))
            .expect_err("an uncorrelated tool call must fail closed");

        assert!(matches!(error, PromptError::EmptyToolCallId));
    }

    #[test]
    fn rejects_arguments_that_are_not_a_json_object() {
        let model = ScriptedModel::new([AssistantTurn {
            content: None,
            tool_calls: vec![ModelToolCall {
                id: "call-1".to_owned(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: Value::String("echo hi".to_owned()).to_string(),
                },
            }],
            replay_items: Vec::new(),
        }]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "malformed", None, limits(1, 32))
            .expect_err("non-object arguments must fail closed");

        assert!(matches!(error, PromptError::ArgumentsNotObject { .. }));
    }
}
