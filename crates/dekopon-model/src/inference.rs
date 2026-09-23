//! Async generation boundary and the closed configured-client dispatch.

use crate::{
    codex::CodexClient,
    control::TurnControl,
    error::InferenceError,
    model::{AssistantTurn, CompletionOptions, ModelMessage, ModelTool},
    openai::OpenAiClient,
    stream::TurnEvent,
};
use std::{future::Future, ops::ControlFlow};

/// Request-scoped inputs; configured generation settings stay in the client.
pub struct GenerateRequest<'a> {
    /// Portable history and same-client continuations.
    pub messages: &'a [ModelMessage],
    /// Tools available to this call.
    pub tools: &'a [ModelTool],
    /// Request-scoped cache affinity.
    pub options: &'a CompletionOptions,
}

/// One asynchronous exchange that reports safe progress and returns only a complete turn.
pub trait InferenceModel: Send + Sync {
    /// Generates a complete turn, or fails without returning partial executable calls.
    fn generate<'a>(
        &'a self,
        request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        control: &'a TurnControl,
    ) -> impl Future<Output = Result<AssistantTurn, InferenceError>> + Send + 'a;
}

/// Configured inference dialects. Each new arm lands with its adapter.
pub enum ModelClient {
    /// Codex Responses using a ChatGPT subscription credential.
    Codex(CodexClient),
    /// Chat completions at a caller-supplied endpoint, streamed or buffered.
    OpenAiCompatible(OpenAiClient),
    /// OpenRouter chat completions with native reasoning replay.
    OpenRouter(crate::openrouter::OpenRouterClient),
}

impl InferenceModel for ModelClient {
    async fn generate<'a>(
        &'a self,
        request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        control: &'a TurnControl,
    ) -> Result<AssistantTurn, InferenceError> {
        match self {
            Self::Codex(client) => client.generate(request, observe, control).await,
            Self::OpenAiCompatible(client) => client.generate(request, observe, control).await,
            Self::OpenRouter(client) => client.generate(request, observe, control).await,
        }
    }
}
