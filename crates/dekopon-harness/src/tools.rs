//! Model-facing tools and bounded request-local asset/image services.

use crate::{meta::AgentConfigView, session::PromptError};
use dekopon_model::{
    image::{GeneratedImage, ImageGenerator, MAX_IMAGE_PROMPT_BYTES},
    model::{ContentPart, ModelMessage, ModelTool, ModelToolCall},
};
use dekopon_shell::ScriptOutcome;
use serde_json::{Value, json};
use std::{fmt, sync::Mutex};

/// Model-facing name of the single scripting tool.
///
/// Named for what it resembles rather than what it is. Models have overwhelming priors about a
/// tool called `bash`, and almost all of them transfer: pipelines, `&&`, `$( )`, exit codes. The
/// description below spends its length on the places where those priors are wrong.
pub const SCRIPT_TOOL_NAME: &str = "bash";

/// The tool a model calls to inspect this session's credential-free agent configuration.
pub const AGENT_CONFIG_TOOL_NAME: &str = "inspect_agent_config";

/// The tool a model calls to look at something a person attached to their message.
pub const ASSET_TOOL_NAME: &str = "fetch_chat_asset";

/// The tool a model calls to create one image for its final chat reply.
pub const IMAGE_GENERATION_TOOL_NAME: &str = "generate_image";

/// The tool an optional chat continuation may call to post nothing.
pub const DECLINE_REPLY_TOOL_NAME: &str = "decline_chat_reply";

/// Tool calls a single model turn may request.
///
/// This bound used to cover one capability invocation each, so 32 was a statement about how much
/// provider work one turn could drive. With one scripting tool it no longer is: a single script
/// can drive many invocations, so the real work bound moved to
/// [`crate::session::PromptLimits::max_capability_calls`], which the interpreter enforces per script and this loop
/// enforces across the session.
///
/// What is left is a well-formedness bound. A scripting tool expresses a multi-step plan *inside*
/// one script, while embedder-owned meta tools can legitimately fan out over a bounded attachment
/// set. Ten calls leave room for that parallel work; anything beyond ten is a runaway rather than
/// a plan.
pub(crate) const MAX_TOOL_CALLS_PER_TURN: usize = 10;

/// Text one chat asset may contribute to the prompt.
///
/// A textual asset arrives as a tool result, and the other tool result a session produces — a
/// script's combined output — is already capped at this exact ceiling by the interpreter. A
/// gateway's own asset budget is sized for images on the wire (8 MiB), which as `text/plain` is
/// roughly two million tokens: handing that to a provider ends the session with a context-length
/// rejection instead of an answer, which is precisely what the asset design refuses to do.
pub(crate) const MAX_TEXTUAL_ASSET_BYTES: usize = dekopon_shell::DEFAULT_MAX_OUTPUT_BYTES;
/// Trusted request-scoped guidance for an unaddressed continuation in an owned chat thread.
pub(crate) const OPTIONAL_REPLY_INSTRUCTION: &str = "This message is an unaddressed continuation inside a \
chat thread the agent already owns. Reply when doing so would materially help. If no response is \
needed—for example, the people are talking to each other, acknowledged the result, or already \
resolved the point—call `decline_chat_reply` instead. That call posts nothing to chat. Do not reply \
merely to have the last word.";

/// A decline after provider work would hide something the session already did.
pub(crate) const DECLINE_AFTER_WORK_RESULT: &str = "A chat reply is required because this session already \
invoked a capability. No tool calls from this turn were run. Provide a concise reply describing \
what happened instead.";

/// One attachment, fetched.
#[derive(Clone, Eq, PartialEq)]
pub struct FetchedAsset {
    /// The name the sender gave it.
    pub name: String,
    /// IANA media type.
    pub mime: String,
    /// The bytes themselves.
    pub data: Vec<u8>,
}

impl fmt::Debug for FetchedAsset {
    /// Summarised rather than printed, for the same reason [`ContentPart`] is.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FetchedAsset")
            .field("name", &self.name)
            .field("mime", &self.mime)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// The attachments one conversation can show a model.
///
/// Deliberately pull rather than push. A screenshot costs tokens on every turn it appears in, and
/// most turns do not need to look at it — so the prompt carries a one-line reference and the model
/// spends the bytes only when it decides the answer depends on them.
///
/// Every refusal is a `String` the model reads, never an error that ends the session: an asset that
/// is too large, expired, or simply not there is something a model can work around by saying so,
/// and killing a session over it would turn a recoverable answer into silence. The implementation
/// owns its own budget for the same reason the shell runtime owns its capability budget.
pub trait AssetSource {
    /// Returns one asset's bytes, or a reason the model can read.
    fn fetch(&self, id: u64) -> Result<FetchedAsset, String>;

    /// Whether this conversation has any attachments at all.
    ///
    /// The tool is not offered when it answers `true`, because a tool that can only fail is a tool
    /// a model will still try.
    fn is_empty(&self) -> bool;
}

/// Request-local slot through which one generated image leaves the prompt loop.
///
/// The bytes never become a model message or part of [`crate::session::SessionExit`]. An embedder takes the slot
/// only after a successful session and drops it on failure or cancellation, which keeps generated
/// content out of transcripts, persistent history, and accidental `Debug` output.
#[derive(Default)]
pub struct GeneratedImageOutput(Mutex<Option<GeneratedImage>>);

impl GeneratedImageOutput {
    /// Removes the generated image, if this session produced one.
    pub fn take(&self) -> Option<GeneratedImage> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub(crate) fn store(&self, image: GeneratedImage) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(image);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ImageGeneration<'a> {
    pub(crate) generator: &'a dyn ImageGenerator,
    pub(crate) output: &'a GeneratedImageOutput,
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

/// Records one model-authored tool call the loop refused to run.
///
/// Every caller passes a fixed category rather than the model's own text: a rejection event is
/// triggered by untrusted model output, and `docs/observability.md` keeps that output out of
/// exported telemetry.
pub(crate) fn reject_tool_call(model_turn: u32, tool_call_index: usize, error_type: &'static str) {
    tracing::error!(
        target: "dekopon_harness::audit",
        {
            audit.event = "agent.tool.rejected",
            model.turn = model_turn,
            tool_call.index = tool_call_index,
            error.type = error_type,
        },
        "model tool call rejected"
    );
}

/// Builds the scripting tool every prompt session offers.
///
/// `command_words` are the words loaded providers contribute on top of the fixed builtins. They are
/// appended rather than interpolated into the prose so the description stays one constant plus a
/// list, and so a session with no providers reads exactly as it did before.
pub(crate) fn script_tool(command_words: &[String]) -> ModelTool {
    let mut description = SCRIPT_TOOL_DESCRIPTION.to_owned();
    if !command_words.is_empty() {
        // Sorted and deduplicated: the tool definition is part of the cached prompt prefix, and
        // provider load order must not produce two definitions for one set of words.
        let mut words = command_words.to_vec();
        words.sort();
        words.dedup();
        description.push_str(&format!(
            "\n\nThis session's providers add these command words: {}. Each behaves like its own command-line program: run `<word> --help` to see its subcommands and flags; `cap --describe` does not cover them.",
            words.join(", ")
        ));
    }
    ModelTool {
        name: SCRIPT_TOOL_NAME.to_owned(),
        description,
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

pub(crate) fn decline_reply_tool() -> ModelTool {
    ModelTool {
        name: DECLINE_REPLY_TOOL_NAME.to_owned(),
        description: "Post nothing to chat and end this optional continuation. Call this instead \
                      of writing text when a reply would not materially help or would merely take \
                      the last word. Call it before running capabilities; once capability work has \
                      happened, a concise report is required."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn agent_config_tool() -> ModelTool {
    ModelTool {
        name: AGENT_CONFIG_TOOL_NAME.to_owned(),
        description: "Inspect this session's credential-free agent configuration. Call this when \
                      asked about the agent's prompt, configuration, Cedar policy, permissions, \
                      tools, limits, or memory. The result contains the exact standing \
                      instructions, route/session bounds, and only the capabilities Cedar \
                      currently grants this sender through this agent. Present it as concise \
                      Markdown tables unless raw JSON was requested. Raw Cedar source, policy \
                      identifiers, principals, subjects, endpoints, paths, legacy credential \
                      names, private secret-map inventory, and all credential values are \
                      intentionally omitted. A public DRN may appear only when the operator put \
                      that inert name in the standing instructions."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    }
}

/// What a repeated `inspect_agent_config` call is answered with.
///
/// The configuration cannot change inside one session — it is built once, from one fresh broker
/// answer — so a second copy would say exactly what the first said. It would also stay in the
/// message vector and be re-sent to the provider on every remaining turn, which is why the
/// repeat is a pointer rather than a bounded-but-large duplicate.
pub(crate) const AGENT_CONFIG_ALREADY_SHOWN: &str = "This session's agent configuration is already in this \
                                          conversation, in the earlier inspect_agent_config \
                                          result. It cannot change within a session; read that \
                                          result again.";

/// Answers one `inspect_agent_config` call without touching the capability budget or broker.
///
/// `already_shown` is the session's own record of whether a full copy is already in `messages`.
/// Inspection stays repeatable under the loop's shared bounds; only the *bytes* are spent once.
pub(crate) fn inspect_agent_config_into(
    messages: &mut Vec<ModelMessage>,
    config: &AgentConfigView,
    call: &ModelToolCall,
    model_turn: u32,
    tool_call_index: usize,
    already_shown: &mut bool,
) -> Result<(), PromptError> {
    if let Err(error) = agent_config_argument(&call.function.name, &call.function.arguments) {
        reject_tool_call(model_turn, tool_call_index, error.telemetry_kind());
        return Err(error);
    }
    let result = if *already_shown {
        AGENT_CONFIG_ALREADY_SHOWN.to_owned()
    } else {
        config.tool_result()
    };
    tracing::info!(
        target: "dekopon_harness::audit",
        {
            audit.event = "agent.config.inspected",
            model.turn = model_turn,
            tool_call.index = tool_call_index,
            config.bytes = result.len(),
            config.repeated = *already_shown,
        },
        "agent configuration inspected"
    );
    *already_shown = true;
    messages.push(ModelMessage::tool(call.id.clone(), result));
    Ok(())
}

/// Requires the decline tool's argument object to be exactly empty.
pub(crate) fn decline_reply_argument(tool: &str, arguments: &str) -> Result<(), PromptError> {
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
    if !arguments.is_empty() {
        return Err(PromptError::DeclineReplyArgumentsNotEmpty {
            tool: tool.to_owned(),
        });
    }
    Ok(())
}

/// Requires the meta tool's argument object to be exactly empty.
pub(crate) fn agent_config_argument(tool: &str, arguments: &str) -> Result<(), PromptError> {
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
    if !arguments.is_empty() {
        return Err(PromptError::AgentConfigArgumentsNotEmpty {
            tool: tool.to_owned(),
        });
    }
    Ok(())
}

/// Media types whose bytes are readable as a tool result rather than as an attachment.
///
/// A model reads these as text, so routing them through an attachment part would encode a file it
/// could simply have been handed. Everything else — an image, a PDF, an office document — has to
/// arrive as a content part instead.
pub(crate) fn is_textual(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json" | "application/xml" | "application/x-yaml" | "application/yaml"
        )
}

pub(crate) fn image_generation_tool() -> ModelTool {
    ModelTool {
        name: IMAGE_GENERATION_TOOL_NAME.to_owned(),
        description:
            "Generate one PNG to attach to your final chat reply. Call this only when the \
                      user asks you to create or draw an image. The session permits one attempt. \
                      After it succeeds, finish with a short textual caption; the gateway delivers \
                      the image separately from your text."
                .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "maxLength": MAX_IMAGE_PROMPT_BYTES,
                    "description": "A self-contained visual description for the image generator."
                }
            },
            "required": ["prompt"],
            "additionalProperties": false
        }),
    }
}

/// Executes the route's one image-generation attempt without putting bytes into model messages.
pub(crate) fn generate_image_into(
    messages: &mut Vec<ModelMessage>,
    generation: ImageGeneration<'_>,
    attempted: &mut bool,
    call: &ModelToolCall,
    model_turn: u32,
    tool_call_index: usize,
    journal: &crate::checkpoint::ExecutionJournal<'_>,
) -> Result<(), PromptError> {
    let prompt = match image_prompt_argument(&call.function.name, &call.function.arguments) {
        Ok(prompt) => prompt,
        Err(error) => {
            reject_tool_call(model_turn, tool_call_index, error.telemetry_kind());
            return Err(error);
        }
    };
    if *attempted {
        tracing::info!(
            target: "dekopon_harness::audit",
            {
                audit.event = "agent.image_generation.refused",
                model.turn = model_turn,
                tool_call.index = tool_call_index,
                reason = "session-limit",
            },
            "image generation refused"
        );
        messages.push(ModelMessage::tool(
            call.id.clone(),
            "This session has already used its one image-generation attempt. Finish with the image already queued, or answer without one.",
        ));
        return Ok(());
    }
    *attempted = true;
    if journal.cancelled() {
        return Err(PromptError::Cancelled);
    }
    let (backend, model) = generation.generator.model_identity();
    let recorder = crate::accounting::CallRecorder::new(
        journal,
        crate::control::ModelIdentity {
            configured: None,
            backend: backend.to_owned(),
            model: model.to_owned(),
            effort: dekopon_core::Effort::ProviderDefault,
        },
        crate::accounting::CallKind::Image,
        model_turn,
    )?;
    let span = recorder.span();
    let result = span.in_scope(|| generation.generator.generate(&prompt, &recorder));
    let cancelled = journal.cancelled();
    recorder.finish(
        if cancelled {
            crate::accounting::CallOutcome::Cancelled
        } else if result.is_ok() {
            crate::accounting::CallOutcome::Succeeded
        } else {
            crate::accounting::CallOutcome::Failed
        },
        result.as_ref().err().map_or("completed", |e| e.category()),
        false,
    )?;
    match result {
        Ok(image) => {
            if journal.cancelled() {
                return Err(PromptError::Cancelled);
            }
            generation.output.store(image);
            messages.push(ModelMessage::tool(
                call.id.clone(),
                "Generated one image for delivery with your final chat reply.",
            ));
        }
        Err(error) => {
            if matches!(
                error,
                dekopon_model::image::ImageGenerationError::Accounting(_)
            ) {
                return Err(crate::session::PromptError::Accounting(
                    dekopon_model::usage::AccountingError("image accounting"),
                ));
            }
            // Fixed gateway-authored text: provider diagnostics can contain reflected prompt text
            // and never belong in the next model request.
            messages.push(ModelMessage::tool(
                call.id.clone(),
                "Image generation failed. Answer without an image.",
            ));
        }
    }
    Ok(())
}

pub(crate) fn image_prompt_argument(tool: &str, arguments: &str) -> Result<String, PromptError> {
    let arguments = serde_json::from_str::<Value>(arguments).map_err(|source| {
        PromptError::InvalidArguments {
            tool: tool.to_owned(),
            source,
        }
    })?;
    let Value::Object(mut arguments) = arguments else {
        return Err(PromptError::ArgumentsNotObject {
            tool: tool.to_owned(),
        });
    };
    let prompt = arguments
        .remove("prompt")
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or_else(|| PromptError::MissingImagePrompt {
            tool: tool.to_owned(),
        })?;
    if !arguments.is_empty() {
        return Err(PromptError::UnexpectedImageArguments {
            tool: tool.to_owned(),
        });
    }
    if prompt.len() > MAX_IMAGE_PROMPT_BYTES {
        return Err(PromptError::ImagePromptTooLarge {
            actual: prompt.len(),
            maximum: MAX_IMAGE_PROMPT_BYTES,
        });
    }
    Ok(prompt)
}

pub(crate) fn asset_tool() -> ModelTool {
    ModelTool {
        name: ASSET_TOOL_NAME.to_owned(),
        description: "Look at a file someone attached to their chat message. The conversation \
                      names each one as `Chat Asset #N`; pass that number. Call this when \
                      answering depends on what the file actually contains."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "description": "The number from the `Chat Asset #N` reference in the conversation."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
    }
}

/// Answers one `fetch_chat_asset` call by appending the tool result and, when the asset is not
/// text, the message that actually carries it.
///
/// Two messages rather than one because **a tool result cannot carry an attachment**. Chat
/// Completions types a `tool` message's content as a string, and the Responses API types
/// `function_call_output.output` the same way; neither accepts an image part where a tool result
/// goes. So the tool result says what happened and a following `user` message carries the bytes.
/// This shape is the only one both wire formats accept — do not "simplify" it by attaching to the
/// tool result.
pub(crate) fn fetch_asset_into(
    messages: &mut Vec<ModelMessage>,
    source: &dyn AssetSource,
    call: &ModelToolCall,
    model_turn: u32,
    tool_call_index: usize,
) -> Result<(), PromptError> {
    let id = match asset_argument(&call.function.name, &call.function.arguments) {
        Ok(id) => id,
        Err(error) => {
            reject_tool_call(model_turn, tool_call_index, error.telemetry_kind());
            return Err(error);
        }
    };
    let span = tracing::info_span!(
        "prompt.asset_fetch",
        model.turn = model_turn,
        tool_call.index = tool_call_index,
        asset.id = id,
    );
    let _entered = span.enter();
    // A refusal is an outcome the model reads, not a failed session. Its text is gateway-authored
    // rather than sender-supplied, so it is safe to record.
    let asset = match source.fetch(id) {
        Ok(asset) => asset,
        Err(reason) => {
            tracing::info!(
                target: "dekopon_harness::audit",
                { audit.event = "agent.asset.refused", asset.id = id, reason = reason.as_str() },
                "chat asset refused"
            );
            messages.push(ModelMessage::tool(call.id.clone(), reason));
            return Ok(());
        }
    };
    let text = is_textual(&asset.mime).then(|| String::from_utf8_lossy(&asset.data).into_owned());
    let truncated = text
        .as_ref()
        .is_some_and(|text| text.len() > MAX_TEXTUAL_ASSET_BYTES);
    // Size and media type, never the bytes and never the sender's file name, which is untrusted.
    tracing::info!(
        target: "dekopon_harness::audit",
        {
            audit.event = "agent.asset.fetched",
            asset.id = id,
            asset.mime = asset.mime.as_str(),
            asset.bytes = asset.data.len(),
            asset.truncated = truncated,
        },
        "chat asset fetched"
    );
    if let Some(text) = text {
        messages.push(ModelMessage::tool(
            call.id.clone(),
            clamp_textual_asset(text),
        ));
        return Ok(());
    }
    messages.push(ModelMessage::tool(
        call.id.clone(),
        format!("Chat Asset #{id} follows in the next message."),
    ));
    let part = if asset.mime.starts_with("image/") {
        ContentPart::Image {
            mime: asset.mime,
            data: asset.data,
        }
    } else {
        ContentPart::File {
            name: asset.name,
            mime: asset.mime,
            data: asset.data,
        }
    };
    messages.push(ModelMessage::user_with_parts(vec![
        ContentPart::Text(format!("Chat Asset #{id}:")),
        part,
    ]));
    Ok(())
}

/// Clamps one textual asset to what a prompt can carry, saying so in the text itself.
///
/// The trailer is part of the tool result rather than a separate signal because the model is the
/// one that has to act on it: it can read what it got, and tell the person the rest was too large
/// to look at. That is the asset contract — an unusable attachment is refused in words the model
/// can pass on, never by failing the session.
pub(crate) fn clamp_textual_asset(mut text: String) -> String {
    let total = text.len();
    if total <= MAX_TEXTUAL_ASSET_BYTES {
        return text;
    }
    let mut end = MAX_TEXTUAL_ASSET_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(&format!("\n[truncated at {end} bytes of {total}]"));
    text
}

/// Extracts the `id` argument from one `fetch_chat_asset` call.
pub(crate) fn asset_argument(tool: &str, arguments: &str) -> Result<u64, PromptError> {
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
    // Models write `5` and `"5"` for the same intent, and refusing the second would spend a turn
    // teaching one that the conversation already told it the number.
    let id = arguments.get("id").and_then(|id| match id {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    });
    id.ok_or_else(|| PromptError::MissingAssetId {
        tool: tool.to_owned(),
    })
}

/// Extracts the `script` argument from one model tool call.
pub(crate) fn script_argument(tool: &str, arguments: &str) -> Result<String, PromptError> {
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
pub(crate) const SCRIPT_TOOL_DESCRIPTION: &str = "\
Run one script in Dekopon's sandboxed shell. This is the only way to invoke capabilities: use it \
whenever the task needs data or an action the session's capabilities provide, and write the whole \
job as one script rather than one tool call per step. Send scripts one after another only when \
the next step genuinely depends on a result you cannot know yet. If you do not yet know what this \
session can call, the first script is `cap --list`. Returns the script's combined output followed \
by an `[exit code: N]` trailer, exactly as a terminal would.

The dialect is eerily close to bash and explicitly not bash. Pipelines, `&&`, `||`, `;`, a \
leading `!`, `if`/`elif`/`else`, `for`, `while`, `until`, `case`/`esac`, `[[ ... ]]`, `{ ...; }` \
groups — the compound ones all usable as pipeline stages, so `cmd | while ...; do ...; done` \
works and a piped loop keeps what it assigns because nothing here forks — `break`/`continue`, \
functions with `$1`/`$@`/`$#`/`shift`/`getopts`/`local`, `read`, `$NAME`, `${NAME[index]}`, \
`${NAME[@]}`, `${#NAME}`, `${NAME:-default}` and its `:=`/`:?`/`:+`/`#`/`%`/`/` relatives, `$( \
)`, `$(( ))`, `$?`, `${PIPESTATUS[@]}`, `set -e`/`set -u`/`set -o pipefail`, `return`, `exit`, \
both quoting forms, here-documents (`<<EOF`, `<<-EOF`, and literal `<<'EOF'`), and redirection of \
either stream (`>`, `>>`, `2>`, `2>>`, `&>`, `2>&1`, `>&2`, `> /dev/null`) into named in-memory \
buffers all behave the way you expect. Everything outside that curated set fails loudly and by \
name: `eval`, backticks, subshells, `<<<`, and `&` backgrounding are errors, never silent no-ops. \
If a script ran, it did what it said.

Four things genuinely differ from a real shell:

1. Commands are Dekopon capabilities, not programs. A command word containing `.`, `-`, or `_` is \
a capability invocation; every other word is a builtin. There are no processes, no filesystem, no \
environment variables, and no network reachable except through a capability. The capabilities you \
may invoke are exactly those this session was granted: no flag, retry, or rewording escalates \
past that set, and a refusal is a fact to report, not an obstacle to work around.
2. Capability arguments are `--kebab-case` flags that become one JSON object. With a capability \
such as `posts.get`, `posts.get --post-id 7 --include-body` sends `{\"postId\": 7, \
\"includeBody\": true}`: a value that reads as a JSON number, `true`, `false`, or `null` is sent \
typed, anything else is sent as a string, and a flag with no value is `true`. A repeated flag \
becomes an array, and a single bare `{...}` argument is used as the input verbatim. `cap \
<capability> ...` invokes one under the same argument rules.
3. Values are JSON, not text. `|` hands a structured value to the next command, and `jq` is built \
in to work on it. A command writes its value to stdout and its diagnostics to stderr, so \
`x=$(cmd)` captures the value while errors still reach you, and `x=$(cmd 2>&1)` is how you \
capture the error text itself. Merging only happens when there is a diagnostic: `cmd 2>&1` on a \
quiet command leaves its value, and its type, untouched.
4. The session is bounded. Steps, output, wall-clock time, and capability calls all have \
ceilings; tripping one ends the script with a message naming it. Filter with `jq`, loop in the \
shell, and print only what you need next.

Builtins: `jq`, `curl`, `cap`, `cat`, `echo`, `printf`, `test`/`[`, `true`, `false`, `sleep`, \
`date`, `grep`, `sed`, `cut`, `sort`, `uniq`, `wc`, `base64`, `xargs`. Two of them depend on \
session configuration and report their exact missing prerequisite otherwise: `curl`, which opens \
no socket of its own but assembles a request for whichever HTTP capability the session was given; \
and `date`, which reads the host clock and renders `+%s` or an ISO-8601 instant. A provider may \
contribute further command words, which behave the same way and are authorized identically; any \
this session has are listed at the end of this description.

A public secret DRN supplied in your instructions is a name, not a value or grant. Use it only in \
exact broker-backed forms: `curl --oauth2-bearer '${drn:...}' URL` or `curl -u 'USER:${drn:...}' \
URL`. Literal passwords, DRNs in headers/URLs/bodies, and DRN concatenation are rejected. The \
provider never receives the DRN, and the broker independently authorizes every use.

Patterns are literal text, never globs, and regular expressions only where you ask for one with \
`-E`: a `grep`/`sed` pattern, a `${NAME#p}`/`${NAME%p}`/`${NAME/p/r}` pattern, the right operand \
of `==` inside `[[ ]]`, and a `case` pattern too, where `*)` remains the default branch but \
`*.json)` is an error rather than a silent mismatch. `grep -E '[0-9]'` and `sed -E 's/^ *//'` are \
how you get a real regular expression, and the only way: unflagged, both are a usage error naming \
the metacharacter rather than a search that quietly finds nothing. Under `-E`, anchors and `.` \
mean what they mean in any regex, but the replacement half of `sed` is still literal text, so \
groups select and do not substitute. `${#NAME}` counts characters of a string but elements of an \
array and keys of an object, because values here are real JSON. Use `jq` when the thing you want \
is structure rather than lines. A here-document's body arrives as one JSON string, so pipe it \
through `jq fromjson` when you want structure out of it.

Reading the result. The tool result is your only evidence: what a script printed is what you \
know, and what it did not print you do not know, so never guess what a capability returned, what \
it accepts, or whether it exists. Exit 0 is success. Exit 1 is a command that ran and failed; a \
capability's error arrives on stderr as `<capability>: failed: ...`, so read it before retrying. \
Exit 127 (`command not found` or `capability not found`) means the word is misspelled or names a \
capability this session does not hold; the two are deliberately indistinguishable, and `cap \
--list` is the fix, not guessing at more names. Exit 126 means this session holds the capability \
but authorization refused this use; different arguments will not change that, so report it. Exit \
2 is a parse error, a refused construct, a usage error, or an exhausted budget, and the message \
names which. Exit 124 is the wall-clock deadline. Output past the ceiling is truncated in the \
middle, keeping the head and the tail with a marker giving the total line count, so filter inside \
the script rather than printing everything and reading it here. Each script starts empty: nothing \
an earlier script assigned survives, but everything it printed is already in this conversation.

Not for: skills, chat attachments, and this agent's own configuration are not files here, and \
when the session offers a tool for one of them it is listed beside this one. There are no files \
at all: `ls` and `cd` do not exist, and `cat` only passes along what is piped or here-documented \
into it.

There is no `help`. The initial session context lists the available capabilities with their \
descriptions and complete input schemas; use it without a discovery call. `cap --list` and \
`cap --describe <capability>` remain fallback inspection commands over that same snapshot. Prefer \
a single script that does the whole job over many small ones — that is the entire point of this tool.";
