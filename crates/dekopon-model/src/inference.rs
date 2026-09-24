use crate::{
    codex::CodexClient,
    control::TurnControl,
    error::InferenceError,
    model::{AssistantTurn, CompletionOptions, ModelMessage, ModelTool},
    openai::OpenAiClient,
    stream::TurnEvent,
};
use std::{future::Future, ops::ControlFlow};

pub struct GenerateRequest<'a> {
    pub messages: &'a [ModelMessage],
    pub tools: &'a [ModelTool],
    pub options: &'a CompletionOptions,
}

pub trait InferenceModel: Send + Sync {
    fn generate<'a>(
        &'a self,
        request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        control: &'a TurnControl,
    ) -> impl Future<Output = Result<AssistantTurn, InferenceError>> + Send + 'a;
}

pub enum ModelClient {
    Codex(CodexClient),
    OpenAiCompatible(OpenAiClient),
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
