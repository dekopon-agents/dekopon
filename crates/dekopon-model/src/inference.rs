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

pub(crate) enum ReplayEvent {
    ModelVerified,
    Text(TurnEvent),
}

struct ReplayStream {
    verified: bool,
    held: String,
}

impl ReplayStream {
    fn observe(
        &mut self,
        event: ReplayEvent,
        observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> ControlFlow<()> {
        match event {
            ReplayEvent::ModelVerified => {
                self.verified = true;
                self.flush(observe)
            }
            ReplayEvent::Text(event) => {
                if self.verified {
                    observe(event)
                } else {
                    let TurnEvent::TextDelta(text) = event;
                    self.held.push_str(text.as_str());
                    ControlFlow::Continue(())
                }
            }
        }
    }

    fn flush(
        &mut self,
        observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> ControlFlow<()> {
        if self.held.is_empty() {
            return ControlFlow::Continue(());
        }
        observe(TurnEvent::TextDelta(crate::stream::ModelText::from_model(
            std::mem::take(&mut self.held),
        )))
    }
}

pub(crate) trait ReplayModel {
    fn resolved_key(&self) -> crate::model::ModelKey;
    fn exchange<'a>(
        &'a self,
        request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(ReplayEvent) -> ControlFlow<()> + Send),
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
    let admitted = request
        .messages
        .iter()
        .any(|message| message.admits_opaque(&identity));
    let mut stream = ReplayStream {
        verified: !admitted,
        held: String::new(),
    };
    let result = model
        .exchange(
            GenerateRequest { ..request },
            &mut |event| stream.observe(event, observe),
            control,
            progress,
            &identity,
        )
        .await;
    if admitted
        && !stream.verified
        && matches!(
            result,
            Err(InferenceError::Protocol(
                crate::error::ProtocolFailure::ContinuationMismatch
            ))
        )
    {
        let portable: Vec<_> = request
            .messages
            .iter()
            .map(|message| match message {
                ModelMessage::Assistant { turn } => {
                    crate::model::assistant_message(&turn.without_opaque())
                }
                ModelMessage::Journal { message } => ModelMessage::Journal {
                    message: message.without_opaque(),
                },
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
                &mut |event| match event {
                    ReplayEvent::ModelVerified => ControlFlow::Continue(()),
                    ReplayEvent::Text(event) => observe(event),
                },
                control,
                progress,
                &identity,
            )
            .await
    } else {
        let stopped = stream.flush(observe).is_break();
        if result.is_ok() && stopped {
            Err(InferenceError::Cancelled)
        } else {
            result
        }
    }
}

#[cfg(test)]
mod tests;
