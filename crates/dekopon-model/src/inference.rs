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

impl ModelClient {
    pub fn model_key(&self) -> crate::model::ModelKey {
        match self {
            Self::Codex(client) => client.model_key(),
            Self::OpenRouter(client) => client.model_key(),
            Self::OpenAiCompatible(client) => client.model_key(),
        }
    }
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

pub(crate) trait ReplayModel {
    fn resolved_key(&self) -> crate::model::ModelKey;
    fn exchange<'a>(
        &'a self,
        request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        control: &'a TurnControl,
        progress: &'a mut crate::http::Progress,
        identity: &'a crate::model::ModelKey,
    ) -> impl Future<Output = Result<AssistantTurn, InferenceError>> + Send + 'a;
}

pub(crate) async fn retry_opaque(
    model: &impl ReplayModel,
    request: GenerateRequest<'_>,
    observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    control: &TurnControl,
    progress: &mut crate::http::Progress,
) -> Result<AssistantTurn, InferenceError> {
    let identity = model.resolved_key();
    if !request.messages.iter().any(|message| matches!(message, ModelMessage::Assistant { turn } if turn.model_key() == Some(&identity) && turn.has_opaque_reasoning())) {
        return model.exchange(request, observe, control, progress, &identity).await;
    }
    let mut events = Vec::new();
    let result = model
        .exchange(
            GenerateRequest { ..request },
            &mut |event| {
                events.push(event);
                ControlFlow::Continue(())
            },
            control,
            progress,
            &identity,
        )
        .await;
    if matches!(
        result,
        Err(InferenceError::Protocol(
            crate::error::ProtocolFailure::ContinuationMismatch
        ))
    ) {
        let portable: Vec<_> = request
            .messages
            .iter()
            .map(|message| match message {
                ModelMessage::Assistant { turn } => {
                    crate::model::assistant_message(&turn.without_opaque())
                }
                ModelMessage::System { .. }
                | ModelMessage::User { .. }
                | ModelMessage::ToolResults { .. } => message.clone(),
            })
            .collect();
        model
            .exchange(
                GenerateRequest {
                    messages: &portable,
                    ..request
                },
                observe,
                control,
                progress,
                &identity,
            )
            .await
    } else {
        if result.is_ok() {
            for event in events {
                if observe(event).is_break() {
                    return Err(InferenceError::Cancelled);
                }
            }
        }
        result
    }
}
