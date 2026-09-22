//! Bridge from the blocking prompt session into its explicitly supplied runtime.

use crate::{
    control::TurnControl,
    error::InferenceError,
    inference::{GenerateRequest, InferenceModel, ModelClient},
    model::{AssistantTurn, ChatModel, CompletionOptions, ModelMessage, ModelTool},
    stream::TurnEvent,
};
use std::{ops::ControlFlow, sync::Arc, time::Duration};
use tokio::{runtime::Handle, sync::watch};

/// Per-session bridge; the expensive configured client is shared, cancellation is not.
pub struct BlockingModel {
    client: Arc<ModelClient>,
    runtime: Handle,
    cancel: watch::Receiver<bool>,
    timeout: Duration,
}

impl BlockingModel {
    /// Supplies the session runtime and signal. Call `complete` only on a blocking task.
    #[must_use]
    pub fn new(
        client: Arc<ModelClient>,
        runtime: Handle,
        cancel: watch::Receiver<bool>,
        timeout: Duration,
    ) -> Self {
        Self {
            client,
            runtime,
            cancel,
            timeout,
        }
    }
}

impl ChatModel for BlockingModel {
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError> {
        let control = TurnControl::new(self.cancel.clone(), self.timeout)?;
        self.runtime.block_on(self.client.generate(
            GenerateRequest {
                messages,
                tools,
                options,
            },
            on_event,
            &control,
        ))
    }
}
