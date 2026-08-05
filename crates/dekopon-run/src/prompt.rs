use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

use dekopon_core::CapabilityId;
use dekopon_model::model::{ChatModel, ModelError, ModelMessage, ModelTool, assistant_message};
use dekopon_provider_host::{ProviderHostError, ProviderRegistry};
use serde_json::Value;
use thiserror::Error;

const MAX_FUNCTION_NAME_LENGTH: usize = 64;
const MAX_TOOL_CALLS_PER_TURN: usize = 32;

/// One capability available to a prompt session.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeTool {
    /// Canonical Dekopon capability ID.
    pub capability: CapabilityId,
    /// Prompt-visible operation description.
    pub description: String,
    /// Object-shaped JSON Schema for arguments.
    pub input_schema: Value,
}

/// Capability execution boundary consumed by the prompt loop.
pub trait ToolRuntime {
    /// Returns all tools available to this session.
    fn tools(&self) -> Vec<RuntimeTool>;

    /// Invokes one canonical capability with untrusted model arguments.
    fn invoke(&self, capability: &CapabilityId, input: &Value) -> Result<Value, ToolRuntimeError>;
}

impl ToolRuntime for ProviderRegistry {
    fn tools(&self) -> Vec<RuntimeTool> {
        self.capabilities()
            .map(|(_provider, capability)| RuntimeTool {
                capability: capability.id.clone(),
                description: capability.description.clone(),
                input_schema: capability.input_schema.clone(),
            })
            .collect()
    }

    fn invoke(&self, capability: &CapabilityId, input: &Value) -> Result<Value, ToolRuntimeError> {
        ProviderRegistry::invoke(self, capability, input)
            .map(|result| result.output)
            .map_err(ToolRuntimeError::Host)
    }
}

/// Result of a completed immediate prompt/tool loop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptOutcome {
    /// Final assistant text.
    pub answer: String,
    /// Number of model requests made.
    pub model_turns: u32,
    /// Number of provider invocations made.
    pub provider_invocations: u32,
}

/// Runs a bounded prompt/tool loop.
pub fn run_prompt<M, R>(
    model: &M,
    runtime: &R,
    prompt: &str,
    system: Option<&str>,
    max_steps: u32,
) -> Result<PromptOutcome, PromptError>
where
    M: ChatModel + ?Sized,
    R: ToolRuntime,
{
    if max_steps == 0 {
        return Err(PromptError::ZeroSteps);
    }

    let tools = bind_tools(runtime.tools())?;
    let model_tools = tools
        .values()
        .map(|tool| tool.model.clone())
        .collect::<Vec<_>>();
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.push(ModelMessage::system(system));
    }
    messages.push(ModelMessage::user(prompt));

    let session_span = tracing::info_span!(
        "prompt.session",
        prompt.max_steps = max_steps,
        tool.count = model_tools.len()
    );
    let _session = session_span.enter();
    tracing::info!(
        audit.event = "agent.session.started",
        prompt.max_steps = max_steps,
        tool.count = model_tools.len(),
        "prompt session started"
    );
    let mut provider_invocations = 0_u32;

    for model_turns in 1..=max_steps {
        let model_span = tracing::info_span!("prompt.model_turn", model.turn = model_turns);
        let model_entered = model_span.enter();
        tracing::info!(
            audit.event = "agent.model.requested",
            model.turn = model_turns,
            message.count = messages.len(),
            tool.count = model_tools.len(),
            "model turn requested"
        );
        let model_started = Instant::now();
        let turn = match model.complete(&messages, &model_tools) {
            Ok(turn) => turn,
            Err(error) => {
                tracing::error!(
                    audit.event = "agent.model.completed",
                    model.turn = model_turns,
                    duration_ms = model_started.elapsed().as_secs_f64() * 1_000.0,
                    outcome = "failed",
                    "model turn failed"
                );
                return Err(error.into());
            }
        };
        tracing::info!(
            audit.event = "agent.model.completed",
            model.turn = model_turns,
            duration_ms = model_started.elapsed().as_secs_f64() * 1_000.0,
            tool_call.count = turn.tool_calls.len(),
            answer.present = turn
                .content
                .as_ref()
                .is_some_and(|content| !content.trim().is_empty()),
            outcome = "succeeded",
            "model turn completed"
        );
        drop(model_entered);
        messages.push(assistant_message(&turn));

        if turn.tool_calls.is_empty() {
            let answer = turn
                .content
                .filter(|content| !content.trim().is_empty())
                .ok_or(PromptError::EmptyAnswer)?;
            return Ok(PromptOutcome {
                answer,
                model_turns,
                provider_invocations,
            });
        }
        if turn.tool_calls.len() > MAX_TOOL_CALLS_PER_TURN {
            tracing::error!(
                audit.event = "agent.tool.rejected",
                model.turn = model_turns,
                tool_call.count = turn.tool_calls.len(),
                error.type = "too-many-tool-calls",
                "model tool calls rejected"
            );
            return Err(PromptError::TooManyToolCalls {
                actual: turn.tool_calls.len(),
                maximum: MAX_TOOL_CALLS_PER_TURN,
            });
        }

        for (tool_call_index, call) in turn.tool_calls.into_iter().enumerate() {
            let tool_call_index = tool_call_index + 1;
            if call.id.trim().is_empty() {
                tracing::error!(
                    audit.event = "agent.tool.rejected",
                    model.turn = model_turns,
                    tool_call.index = tool_call_index,
                    error.type = "empty-tool-call-id",
                    "model tool call rejected"
                );
                return Err(PromptError::EmptyToolCallId);
            }
            let Some(tool) = tools.get(&call.function.name) else {
                tracing::error!(
                    audit.event = "agent.tool.rejected",
                    model.turn = model_turns,
                    tool_call.index = tool_call_index,
                    error.type = "unknown-tool",
                    "model tool call rejected"
                );
                return Err(PromptError::UnknownTool(call.function.name));
            };
            let arguments = match serde_json::from_str::<Value>(&call.function.arguments) {
                Ok(arguments) => arguments,
                Err(source) => {
                    tracing::error!(
                        audit.event = "agent.tool.rejected",
                        model.turn = model_turns,
                        tool_call.index = tool_call_index,
                        capability.id = %tool.capability,
                        error.type = "invalid-json-arguments",
                        "model tool call rejected"
                    );
                    return Err(PromptError::InvalidArguments {
                        tool: call.function.name,
                        source,
                    });
                }
            };
            if !arguments.is_object() {
                tracing::error!(
                    audit.event = "agent.tool.rejected",
                    model.turn = model_turns,
                    tool_call.index = tool_call_index,
                    capability.id = %tool.capability,
                    error.type = "arguments-not-object",
                    "model tool call rejected"
                );
                return Err(PromptError::ArgumentsNotObject {
                    tool: call.function.name,
                });
            }

            let span = tracing::info_span!(
                "prompt.tool_call",
                tool.name = %tool.model.name,
                capability.id = %tool.capability,
                model.turn = model_turns,
                tool_call.index = tool_call_index
            );
            let output = {
                let _entered = span.enter();
                tracing::info!(
                    audit.event = "agent.tool.invocation.started",
                    capability.id = %tool.capability,
                    model.turn = model_turns,
                    tool_call.index = tool_call_index,
                    "agent tool invocation started"
                );
                let tool_started = Instant::now();
                match runtime.invoke(&tool.capability, &arguments) {
                    Ok(output) => {
                        tracing::info!(
                            audit.event = "agent.tool.invocation.completed",
                            capability.id = %tool.capability,
                            model.turn = model_turns,
                            tool_call.index = tool_call_index,
                            duration_ms = tool_started.elapsed().as_secs_f64() * 1_000.0,
                            outcome = "succeeded",
                            "agent tool invocation completed"
                        );
                        output
                    }
                    Err(error) => {
                        tracing::error!(
                            audit.event = "agent.tool.invocation.completed",
                            capability.id = %tool.capability,
                            model.turn = model_turns,
                            tool_call.index = tool_call_index,
                            duration_ms = tool_started.elapsed().as_secs_f64() * 1_000.0,
                            outcome = "failed",
                            "agent tool invocation failed"
                        );
                        return Err(error.into());
                    }
                }
            };
            provider_invocations = provider_invocations.saturating_add(1);
            let content = serde_json::to_string(&output).map_err(PromptError::ToolResult)?;
            messages.push(ModelMessage::tool(call.id, content));
        }
    }

    Err(PromptError::MaxSteps { maximum: max_steps })
}

#[derive(Clone, Debug)]
struct BoundTool {
    capability: CapabilityId,
    model: ModelTool,
}

fn bind_tools(tools: Vec<RuntimeTool>) -> Result<BTreeMap<String, BoundTool>, PromptError> {
    if tools.is_empty() {
        return Err(PromptError::NoTools);
    }

    let mut tools = tools;
    tools.sort_by(|left, right| left.capability.cmp(&right.capability));
    let mut names = BTreeSet::new();
    let mut bound = BTreeMap::new();

    for tool in tools {
        let base = function_name(tool.capability.as_str());
        let mut name = base.clone();
        let mut discriminator = 2_u32;
        while !names.insert(name.clone()) {
            let suffix = format!("_{discriminator}");
            let prefix_length = MAX_FUNCTION_NAME_LENGTH.saturating_sub(suffix.len());
            name = format!("{}{suffix}", &base[..base.len().min(prefix_length)]);
            discriminator = discriminator.saturating_add(1);
        }

        let description = format!(
            "{} (Dekopon capability: {})",
            tool.description, tool.capability
        );
        bound.insert(
            name.clone(),
            BoundTool {
                capability: tool.capability,
                model: ModelTool {
                    name,
                    description,
                    parameters: tool.input_schema,
                },
            },
        );
    }

    Ok(bound)
}

fn function_name(capability: &str) -> String {
    let mut name = capability
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    name.truncate(MAX_FUNCTION_NAME_LENGTH);
    if name.is_empty() {
        "dekopon_tool".to_owned()
    } else {
        name
    }
}

/// Failure while exposing or executing prompt tools.
#[derive(Debug, Error)]
pub enum ToolRuntimeError {
    /// The Wasm provider host rejected or failed an invocation.
    #[error(transparent)]
    Host(#[from] ProviderHostError),
    /// An alternate runtime rejected an invocation.
    #[error("tool runtime failed: {0}")]
    Other(String),
}

/// Failure to complete a prompt/tool session.
#[derive(Debug, Error)]
pub enum PromptError {
    /// A zero-length loop was requested.
    #[error("prompt max steps must be greater than zero")]
    ZeroSteps,
    /// No tools were available to expose to the model.
    #[error("at least one provider capability is required for prompt mode")]
    NoTools,
    /// A model request failed.
    #[error(transparent)]
    Model(#[from] ModelError),
    /// The model selected a tool that was not offered.
    #[error("model requested unknown tool {0:?}")]
    UnknownTool(String),
    /// A model attempted too many provider calls in one turn.
    #[error("model returned {actual} tool calls in one turn; the maximum is {maximum}")]
    TooManyToolCalls {
        /// Model-requested call count.
        actual: usize,
        /// Fixed immediate-mode bound.
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
    /// Provider execution failed.
    #[error(transparent)]
    Runtime(#[from] ToolRuntimeError),
    /// A provider output could not be serialized for the model.
    #[error("could not serialize provider output for the model")]
    ToolResult(#[source] serde_json::Error),
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
    use std::{cell::RefCell, collections::VecDeque};

    use dekopon_core::CapabilityId;
    use serde_json::{Value, json};

    use dekopon_model::model::{
        AssistantTurn, ChatModel, ModelError, ModelFunctionCall, ModelMessage, ModelTool,
        ModelToolCall,
    };

    use super::{
        PromptError, RuntimeTool, ToolRuntime, ToolRuntimeError, function_name, run_prompt,
    };

    struct ScriptedModel {
        turns: RefCell<VecDeque<AssistantTurn>>,
        observed_message_counts: RefCell<Vec<usize>>,
    }

    impl ChatModel for ScriptedModel {
        fn complete(
            &self,
            messages: &[ModelMessage],
            tools: &[ModelTool],
        ) -> Result<AssistantTurn, ModelError> {
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "echo_echo");
            self.observed_message_counts
                .borrow_mut()
                .push(messages.len());
            self.turns
                .borrow_mut()
                .pop_front()
                .ok_or(ModelError::NoChoices)
        }
    }

    struct RecordingRuntime {
        invocations: RefCell<Vec<(CapabilityId, Value)>>,
    }

    impl ToolRuntime for RecordingRuntime {
        fn tools(&self) -> Vec<RuntimeTool> {
            vec![RuntimeTool {
                capability: "echo.echo".parse().expect("valid fixture"),
                description: "Echoes input".to_owned(),
                input_schema: json!({"type": "object"}),
            }]
        }

        fn invoke(
            &self,
            capability: &CapabilityId,
            input: &Value,
        ) -> Result<Value, ToolRuntimeError> {
            self.invocations
                .borrow_mut()
                .push((capability.clone(), input.clone()));
            Ok(input.clone())
        }
    }

    #[test]
    fn executes_tool_calls_and_returns_the_final_answer() {
        let model = ScriptedModel {
            turns: RefCell::new(VecDeque::from([
                AssistantTurn {
                    content: None,
                    tool_calls: vec![ModelToolCall {
                        id: "call-1".to_owned(),
                        kind: "function".to_owned(),
                        function: ModelFunctionCall {
                            name: "echo_echo".to_owned(),
                            arguments: r#"{"message":"hello"}"#.to_owned(),
                        },
                    }],
                    replay_items: Vec::new(),
                },
                AssistantTurn {
                    content: Some("The provider echoed hello.".to_owned()),
                    tool_calls: Vec::new(),
                    replay_items: Vec::new(),
                },
            ])),
            observed_message_counts: RefCell::new(Vec::new()),
        };
        let runtime = RecordingRuntime {
            invocations: RefCell::new(Vec::new()),
        };

        let outcome =
            run_prompt(&model, &runtime, "say hello", None, 4).expect("prompt session succeeds");

        assert_eq!(outcome.answer, "The provider echoed hello.");
        assert_eq!(outcome.model_turns, 2);
        assert_eq!(outcome.provider_invocations, 1);
        assert_eq!(*model.observed_message_counts.borrow(), vec![1, 3]);
        assert_eq!(runtime.invocations.borrow().len(), 1);
    }

    #[test]
    fn rejects_model_selected_tools_that_were_not_offered() {
        let model = ScriptedModel {
            turns: RefCell::new(VecDeque::from([AssistantTurn {
                content: None,
                tool_calls: vec![ModelToolCall {
                    id: "call-1".to_owned(),
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: "other_tool".to_owned(),
                        arguments: "{}".to_owned(),
                    },
                }],
                replay_items: Vec::new(),
            }])),
            observed_message_counts: RefCell::new(Vec::new()),
        };
        let runtime = RecordingRuntime {
            invocations: RefCell::new(Vec::new()),
        };

        let error = run_prompt(&model, &runtime, "try another tool", None, 1)
            .expect_err("unknown tools must fail closed");

        assert!(matches!(error, PromptError::UnknownTool(_)));
        assert!(runtime.invocations.borrow().is_empty());
    }

    #[test]
    fn bounds_tool_call_fan_out_per_model_turn() {
        let tool_calls = (0..33)
            .map(|index| ModelToolCall {
                id: format!("call-{index}"),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: "echo_echo".to_owned(),
                    arguments: "{}".to_owned(),
                },
            })
            .collect();
        let model = ScriptedModel {
            turns: RefCell::new(VecDeque::from([AssistantTurn {
                content: None,
                tool_calls,
                replay_items: Vec::new(),
            }])),
            observed_message_counts: RefCell::new(Vec::new()),
        };
        let runtime = RecordingRuntime {
            invocations: RefCell::new(Vec::new()),
        };

        let error = run_prompt(&model, &runtime, "fan out", None, 1)
            .expect_err("tool-call fan-out must be bounded");

        assert!(matches!(error, PromptError::TooManyToolCalls { .. }));
        assert!(runtime.invocations.borrow().is_empty());
    }

    #[test]
    fn function_names_are_openai_compatible_and_bounded() {
        assert_eq!(
            function_name("github.pull-request.read"),
            "github_pull_request_read"
        );
        assert_eq!(function_name(&"a".repeat(100)).len(), 64);
    }
}
