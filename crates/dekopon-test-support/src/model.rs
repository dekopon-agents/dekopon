//! A streaming model whose every event is released by the test that is watching for it.

use std::{
    ops::ControlFlow,
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
    time::Duration,
};

use dekopon_model::{
    ModelText, TurnEvent,
    model::{AssistantTurn, ChatModel, CompletionOptions, ModelError, ModelMessage, ModelTool},
};
use tokio::sync::Notify;

/// Longest a scripted stream waits for its next release before the fixture, not the subject, fails.
///
/// This stands in for `timeout_global` on the real client: a Responses stream that goes silent
/// through the reasoning phase is bounded by nothing else, and a test that asserts "cancel lands
/// only at the deadline" needs a deadline that actually arrives. A test whose subject *is* that
/// deadline names a short one of its own through [`ScriptedStreamModel::parked`]; this is the
/// ceiling for every other release, where waiting at all means the test has already failed.
const PARK_CEILING: Duration = Duration::from_secs(30);

/// A [`ChatModel`] that replays recorded stream events one at a time, on demand.
///
/// Streaming is where ordering assertions live: "the progress message was posted before the second
/// delta", "a cancel between deltas stops the turn", "the partial text on screen is the text the
/// loop had". None of those is assertable against a model that answers all at once, so this one
/// hands out exactly one event per release and announces each hand-off.
///
/// The two directions use different primitives for the same reason [`crate::BlockedRuntime`] does:
/// the waiting side of `release` is the blocking prompt thread, which has no executor, while the
/// waiting side of the announcement is an async test that may have time paused.
///
/// A model client cannot be asked to invent visible text: [`ModelText`] is constructed from bytes
/// only inside `dekopon-model`, by its parser. So the scripted events come from a recorded
/// transcript through that same parser, which also means a fixture cannot drift from what the
/// backend really sends.
pub struct ScriptedStreamModel {
    events: Mutex<Vec<TurnEvent>>,
    turn: AssistantTurn,
    gate: Mutex<Receiver<()>>,
    release: Sender<()>,
    asked: Notify,
    emitted: Notify,
    count: AtomicU32,
    /// How long one release may be waited on, which a parked stream's own test chooses.
    deadline: Duration,
}

impl ScriptedStreamModel {
    /// Replays a recorded SSE body's events, one per [`Self::release_next`].
    ///
    /// The turn is what the same response parses to when nothing interrupts it, so a test can
    /// assert that the streamed and non-streamed reads of one transcript agree.
    ///
    /// # Errors
    ///
    /// When the transcript is not a body either backend's parser accepts, which is the fixture
    /// being wrong rather than the subject.
    pub fn from_transcript(body: &str, turn: AssistantTurn) -> Result<Self, ModelError> {
        Ok(Self::scripted(
            dekopon_model::events_from_transcript(body)?,
            turn,
        ))
    }

    /// Replays these events, one per [`Self::release_next`], then answers with `turn`.
    ///
    /// One more release than there are events is needed: the last one lets the turn return, which
    /// is what puts "the stream has ended" under the test's control too.
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

    /// A stream that sends nothing at all until it is released or `deadline` elapses.
    ///
    /// This is the Codex reasoning phase: the request is open, the socket is silent, and the
    /// event-boundary cancellation check has no boundary to run at. A cancel here lands at the
    /// deadline and nowhere earlier, and that is a property to pin rather than to fix — which is
    /// why the deadline is the caller's to choose: it stands in for the client's `timeout_global`,
    /// and a test that has to outlast it cannot spend the fixture's own half-minute ceiling doing
    /// so.
    #[must_use]
    pub fn parked(turn: AssistantTurn, deadline: Duration) -> Self {
        Self {
            deadline,
            ..Self::scripted(Vec::new(), turn)
        }
    }

    /// Lets the stream emit its next event, or return its turn when none are left.
    pub fn release_next(&self) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a stream that has already returned drops its receiver; the assertions on \
                      what it emitted are what report that, not this send"
        )]
        let _ = self.release.send(());
    }

    /// Resolves once one more event has been handed to the prompt loop.
    pub async fn wait_for_event(&self) {
        self.emitted.notified().await;
    }

    /// Resolves once a turn has reached the model, which is when the request is open.
    ///
    /// The rendezvous a silent phase needs: a parked stream announces nothing else, so a test that
    /// slept instead would be asserting on whether its own sleep outran the loop's start-up rather
    /// than on what a stop does to an open request.
    pub async fn wait_until_asked(&self) {
        self.asked.notified().await;
    }

    /// How many events the loop has taken, which is what `stream.deltas` must equal.
    #[must_use]
    pub fn emitted(&self) -> u32 {
        self.count.load(Ordering::SeqCst)
    }

    /// Blocks until the test releases the next step, or the deadline stands in for the client's.
    fn wait_for_release(&self) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "the deadline elapsing is this fixture's stand-in for timeout_global, which \
                      is the outcome the parked-stream test asserts on rather than an error"
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
        on_event: &mut dyn FnMut(TurnEvent) -> ControlFlow<()>,
    ) -> Result<AssistantTurn, ModelError> {
        self.asked.notify_one();
        let scripted = std::mem::take(&mut *self.events.lock().expect("scripted events"));
        for event in scripted {
            self.wait_for_release();
            let flow = on_event(event);
            self.count.fetch_add(1, Ordering::SeqCst);
            self.emitted.notify_one();
            if flow.is_break() {
                // The real client drops the body here, which closes the connection; nothing
                // further is read and no turn exists to report.
                return Err(ModelError::Interrupted);
            }
        }
        self.wait_for_release();
        Ok(self.turn.clone())
    }
}

/// Every scripted text delta's text, concatenated, which is what a finished stream should show.
///
/// Written here rather than in each suite because "what the person would have read" is the same
/// question everywhere, and two suites computing it differently is how a rendering assertion stops
/// meaning anything.
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
