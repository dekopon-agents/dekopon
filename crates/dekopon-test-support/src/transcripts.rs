//! Recorded server-sent-event bodies for the two streaming backends.
//!
//! These are transcripts, not hand-written expectations: each one is the byte stream a backend
//! actually writes for one turn, kept whole so a parser change is measured against what a provider
//! sends rather than against what the previous parser happened to accept. They carry the awkward
//! parts on purpose — the chat-completions role chunk whose `delta.content` is `null`, the `usage:
//! null` vLLM repeats on every chunk, the trailing usage-only chunk with an empty `choices` array,
//! the `[DONE]` sentinel, and on the Responses route the reasoning item that arrives and completes
//! before any visible text does.
//!
//! Each pair covers the two turns the prompt loop distinguishes: one that answers in text, and one
//! that asks for a tool. A streaming parse and a non-streaming parse of the same turn must agree,
//! and these are the inputs that claim is testable against.

/// One OpenAI chat-completions turn streamed as two visible text deltas, then usage.
pub const OPENAI_CHAT_COMPLETIONS_TWO_DELTAS: &str =
    include_str!("../transcripts/openai-chat-completions-two-deltas.sse");

/// One OpenAI chat-completions turn whose only output is a `bash` tool call, arguments in three
/// fragments across the chunks that carry them.
pub const OPENAI_CHAT_COMPLETIONS_TOOL_CALL: &str =
    include_str!("../transcripts/openai-chat-completions-tool-call.sse");

/// One Codex Responses turn: a reasoning item with no summary, then two visible text deltas.
///
/// The gap between `response.output_item.added` for the reasoning item and its `done` is where a
/// real stream goes silent for the whole reasoning phase, which is why a cancel during it lands
/// only at the request deadline.
pub const CODEX_RESPONSES_TWO_DELTAS: &str =
    include_str!("../transcripts/codex-responses-two-deltas.sse");

/// One Codex Responses turn that ends in a `function_call` item with its arguments in one delta.
pub const CODEX_RESPONSES_TOOL_CALL: &str =
    include_str!("../transcripts/codex-responses-tool-call.sse");
