use std::{
    ops::ControlFlow,
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    time::Duration,
};

use dekopon_model::error::InferenceError;
use dekopon_model::{
    ModelText, TurnEvent,
    model::{AssistantTurn, ChatModel, CompletionOptions, ModelMessage, ModelTool},
};
use tokio::sync::Notify;

const PARK_CEILING: Duration = Duration::from_secs(30);

pub struct ScriptedStreamModel {
    events: Mutex<Vec<TurnEvent>>,
    turn: AssistantTurn,
    gate: Mutex<Receiver<()>>,
    release: Sender<()>,
    asked: Notify,
    emitted: Notify,
    count: AtomicU32,
    deadline: Duration,
}

impl ScriptedStreamModel {
    pub fn from_transcript(body: &str, turn: AssistantTurn) -> Result<Self, InferenceError> {
        Ok(Self::scripted(
            dekopon_model::events_from_transcript(body)?,
            turn,
        ))
    }

    #[must_use]
    pub fn scripted(events: Vec<TurnEvent>, turn: AssistantTurn) -> Self {
        let (release, gate) = channel();
        Self {
            events: Mutex::new(events),
            turn,
            gate: Mutex::new(gate),
            release,
            asked: Notify::new(),
            emitted: Notify::new(),
            count: AtomicU32::new(0),
            deadline: PARK_CEILING,
        }
    }

    #[must_use]
    pub fn parked(turn: AssistantTurn, deadline: Duration) -> Self {
        Self {
            deadline,
            ..Self::scripted(Vec::new(), turn)
        }
    }

    pub fn release_next(&self) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a stream that has already returned drops its receiver; the assertions on \
                      what it emitted are what report that, not this send"
        )]
        let _ = self.release.send(());
    }

    pub async fn wait_for_event(&self) {
        self.emitted.notified().await;
    }

    pub async fn wait_until_asked(&self) {
        self.asked.notified().await;
    }

    #[must_use]
    pub fn emitted(&self) -> u32 {
        self.count.load(Ordering::SeqCst)
    }

    fn wait_for_release(&self) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "this fixture's own deadline elapsing is the outcome the parked-stream \
                      test asserts on rather than an error"
        )]
        let _ = self
            .gate
            .lock()
            .expect("stream gate")
            .recv_timeout(self.deadline);
    }
}

impl ChatModel for ScriptedStreamModel {
    fn complete(
        &self,
        _messages: &[ModelMessage],
        _tools: &[ModelTool],
        _options: &CompletionOptions,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError> {
        self.asked.notify_one();
        let scripted = std::mem::take(&mut *self.events.lock().expect("scripted events"));
        for event in scripted {
            self.wait_for_release();
            let flow = on_event(event);
            self.count.fetch_add(1, Ordering::SeqCst);
            self.emitted.notify_one();
            if flow.is_break() {
                return Err(InferenceError::Cancelled);
            }
        }
        self.wait_for_release();
        Ok(self.turn.clone())
    }
}

#[must_use]
pub fn scripted_text(events: &[TurnEvent]) -> ModelText {
    let mut text = ModelText::default();
    for event in events {
        if let TurnEvent::TextDelta(delta) = event {
            text.push(delta);
        }
    }
    text
}
