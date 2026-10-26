use super::*;
use crate::{
    http::Progress,
    model::{ModelKey, assistant_message},
    stream::ModelText,
};

struct CancelledBeforeModel;

impl ReplayModel for CancelledBeforeModel {
    fn resolved_key(&self) -> ModelKey {
        ModelKey::fixture()
    }

    async fn exchange<'a>(
        &'a self,
        _request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(ReplayEvent) -> ControlFlow<()> + Send),
        _control: &'a TurnControl,
        _progress: &'a mut Progress,
        _identity: &'a ModelKey,
    ) -> Result<AssistantTurn, InferenceError> {
        for text in ["partial ", "answer canary"] {
            assert!(
                observe(ReplayEvent::Text(TurnEvent::TextDelta(
                    ModelText::from_model(text.to_owned())
                )))
                .is_continue()
            );
        }
        Err(InferenceError::Cancelled)
    }
}

#[tokio::test]
async fn cancellation_with_admitted_opaque_reasoning_flushes_all_held_text_once() {
    let turn = AssistantTurn::new(None, Vec::new(), None).with_codex_continuation(
        vec![serde_json::json!({"type":"reasoning","encrypted_content":"synthetic-opaque"})],
        ModelKey::fixture(),
        None,
    );
    let messages = [assistant_message(&turn)];
    let options = CompletionOptions::default();
    let control = TurnControl::new(
        tokio::sync::watch::channel(false).1,
        std::time::Duration::from_secs(1),
    )
    .unwrap();
    let mut events = Vec::new();
    let result = retry_opaque(
        &CancelledBeforeModel,
        GenerateRequest {
            messages: &messages,
            tools: &[],
            options: &options,
        },
        &mut |event| {
            events.push(event);
            ControlFlow::Break(())
        },
        &control,
        &mut Progress::new(),
    )
    .await;
    assert!(matches!(result, Err(InferenceError::Cancelled)));
    let [TurnEvent::TextDelta(text)] = events.as_slice() else {
        panic!("one coalesced partial answer");
    };
    assert_eq!(text.as_str(), "partial answer canary");
}

struct MismatchAfterVerification(std::sync::atomic::AtomicUsize);

impl ReplayModel for MismatchAfterVerification {
    fn resolved_key(&self) -> ModelKey {
        ModelKey::fixture()
    }
    async fn exchange<'a>(
        &'a self,
        _request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(ReplayEvent) -> ControlFlow<()> + Send),
        _control: &'a TurnControl,
        _progress: &'a mut Progress,
        _identity: &'a ModelKey,
    ) -> Result<AssistantTurn, InferenceError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _flow = observe(ReplayEvent::ModelVerified);
        let _flow = observe(ReplayEvent::Text(TurnEvent::TextDelta(
            ModelText::from_model("visible".to_owned()),
        )));
        Err(crate::error::ProtocolFailure::ContinuationMismatch.into())
    }
}

#[tokio::test]
async fn a_mismatch_after_verification_never_appends_a_second_attempt() {
    let model = MismatchAfterVerification(std::sync::atomic::AtomicUsize::new(0));
    let turn = AssistantTurn::new(None, Vec::new(), None).with_codex_continuation(
        vec![serde_json::json!({"type":"reasoning"})],
        ModelKey::fixture(),
        None,
    );
    let messages = [assistant_message(&turn)];
    let options = CompletionOptions::default();
    let control = TurnControl::new(
        tokio::sync::watch::channel(false).1,
        std::time::Duration::from_secs(1),
    )
    .unwrap();
    let mut seen = String::new();
    let result = retry_opaque(
        &model,
        GenerateRequest {
            messages: &messages,
            tools: &[],
            options: &options,
        },
        &mut |event| {
            let TurnEvent::TextDelta(text) = event;
            seen.push_str(text.as_str());
            ControlFlow::Continue(())
        },
        &control,
        &mut Progress::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(InferenceError::Protocol(
            crate::error::ProtocolFailure::ContinuationMismatch
        ))
    ));
    assert_eq!(seen, "visible");
    assert_eq!(model.0.load(std::sync::atomic::Ordering::Relaxed), 1);
}
