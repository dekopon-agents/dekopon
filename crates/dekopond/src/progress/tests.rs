//! What the policy puts on screen, on a clock the test drives.
//!
//! Every timing rule here is minutes long in production — a keep-alive at 45 seconds, a coalescing
//! window, a wall-clock budget — so the clock is paused and advanced deliberately. A test that
//! slept would be a test nobody runs.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use dekopon_agent::{
    BudgetLimit, CancelSource, CancelVia, ProgressEvent, ProgressSink, SessionOutcome,
};
use dekopon_model::ModelText;

use crate::{
    config::{
        LivenessMode, LivenessSettings, ProgressSurface, ResolvedLiveness, TemplateOverrides,
    },
    progress::{
        KeepAlive, ProgressDetail, ProgressInputs, ProgressPolicy, Templates, Terminal,
        adapter::ProgressAdapter,
        text::{RenderState, TemplateField},
    },
    session::{FAILURE_REPLY, STOPPED_REPLY, SessionCancellation},
    transport::{
        CancelButton, CancelPress, ChatDriver, InboundReaction, LivenessTarget, MessageRef,
        NativeStatus, OutboundReply, ProgressLimits, ProgressMessage, ReplyTarget, Status,
        StreamLimits, StreamedText, TextStream, TransportError, TypingLease,
    },
};

/// What a driver was asked to do, in order.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Reaction(bool),
    Typing,
    Status(Status),
    Post(String),
    Edit(String),
    Delete,
    Finalize(String),
    Stream(String),
    StreamFinalize(String),
    Reply(String),
}

/// Everything the fake transport recorded, plus the failures a test injects.
#[derive(Debug, Default)]
struct Recorder {
    calls: Mutex<Vec<Call>>,
    /// Progress posts and edits to refuse, counted down.
    refuse_progress: AtomicU32,
    /// Finalize calls to refuse, counted down.
    refuse_finalize: AtomicU32,
    posts: AtomicU32,
}

impl Recorder {
    fn push(&self, call: Call) {
        self.calls.lock().expect("recorder").push(call);
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("recorder").clone()
    }

    fn texts(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Post(text) | Call::Edit(text) | Call::Stream(text) => Some(text),
                _ => None,
            })
            .collect()
    }

    /// Spends one injected failure, reporting whether there was one to spend.
    ///
    /// `checked_sub` rather than a guard around a subtraction: `then_some` evaluates its argument,
    /// so the guarded form still subtracts from zero on the common path where a test injected no
    /// failure at all.
    fn counted(&self, counter: &AtomicU32) -> bool {
        counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }

    /// Everything the driver wrote into the conversation, in order.
    ///
    /// The service's own indicators are left out because they bracket a session on both sides: the
    /// ladder runs before the first write, and `cleanup` returns status and reaction to rest
    /// *after* the ending is delivered, which is what lets an answered session release its
    /// admission permit while that last cosmetic call is still in flight. An assertion about which
    /// message the answer landed in is about this list.
    fn writes(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| !matches!(call, Call::Reaction(_) | Call::Typing | Call::Status(_)))
            .collect()
    }
}

/// Which capability objects this driver offers.
#[derive(Clone, Copy, Debug)]
struct Offers {
    typing: bool,
    status: bool,
    reaction: bool,
    progress: bool,
    stream: bool,
    cancel_button: bool,
}

impl Default for Offers {
    fn default() -> Self {
        Self {
            typing: true,
            status: true,
            reaction: true,
            progress: true,
            stream: false,
            cancel_button: false,
        }
    }
}

/// One object implementing every capability, the way the local reference driver does.
struct Surfaces {
    recorder: Arc<Recorder>,
    min_edit_interval: Duration,
    min_stream_interval: Duration,
}

#[async_trait]
impl TypingLease for Surfaces {
    fn renew_every(&self) -> Duration {
        Duration::from_secs(8)
    }

    async fn renew(&self, _target: &LivenessTarget) -> Result<(), TransportError> {
        self.recorder.push(Call::Typing);
        Ok(())
    }
}

#[async_trait]
impl NativeStatus for Surfaces {
    async fn set(&self, _target: &LivenessTarget, status: Status) -> Result<(), TransportError> {
        self.recorder.push(Call::Status(status));
        Ok(())
    }
}

#[async_trait]
impl InboundReaction for Surfaces {
    async fn set(&self, _target: &LivenessTarget, present: bool) -> Result<(), TransportError> {
        self.recorder.push(Call::Reaction(present));
        Ok(())
    }
}

#[async_trait]
impl CancelButton for Surfaces {
    async fn ack(&self, _press: &CancelPress) -> Result<(), TransportError> {
        Ok(())
    }
}

#[async_trait]
impl ProgressMessage for Surfaces {
    fn limits(&self) -> ProgressLimits {
        ProgressLimits {
            max_chars: 2_000,
            min_edit_interval: self.min_edit_interval,
        }
    }

    async fn post(
        &self,
        target: &LivenessTarget,
        text: &crate::progress::ProgressText,
        _cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        if self.recorder.counted(&self.recorder.refuse_progress) {
            return Err(TransportError::Response);
        }
        self.recorder.push(Call::Post(text.as_str().to_owned()));
        let posts = self.recorder.posts.fetch_add(1, Ordering::Relaxed);
        Ok(MessageRef {
            target: target.clone(),
            id: format!("m{posts}"),
        })
    }

    async fn edit(
        &self,
        _message: &MessageRef,
        text: &crate::progress::ProgressText,
        _cancel: bool,
    ) -> Result<(), TransportError> {
        if self.recorder.counted(&self.recorder.refuse_progress) {
            return Err(TransportError::Response);
        }
        self.recorder.push(Call::Edit(text.as_str().to_owned()));
        Ok(())
    }

    async fn delete(&self, _message: &MessageRef) -> Result<(), TransportError> {
        self.recorder.push(Call::Delete);
        Ok(())
    }

    async fn finalize(
        &self,
        _message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        if self.recorder.counted(&self.recorder.refuse_finalize) {
            return Err(TransportError::Response);
        }
        self.recorder.push(Call::Finalize(reply.text.clone()));
        Ok(())
    }
}

/// What this fake's streamed surface holds, small enough that one recorded turn overflows it.
const STREAM_CEILING: usize = 16;

/// The marker a driver appends when [`StreamedText::truncated`] is set, the same `…` a real
/// transport uses. It belongs to the driver and not to the policy, so the text the policy hands
/// over is model-authored throughout and the marker can never be mistaken for the model's own.
const TRUNCATION_MARKER: &str = "…";

#[async_trait]
impl TextStream for Surfaces {
    fn limits(&self) -> StreamLimits {
        StreamLimits {
            min_interval: self.min_stream_interval,
            max_chars: STREAM_CEILING,
        }
    }

    async fn show(
        &self,
        target: &LivenessTarget,
        message: Option<&MessageRef>,
        text: &StreamedText,
        _cancel: bool,
    ) -> Result<MessageRef, TransportError> {
        // Rendered the way every driver renders it: the bounded text, plus the marker when the
        // policy says it cut the text. Recording the text alone would let a policy that bounded
        // silently pass, and a person would read an answer that stops mid-word.
        let mut rendered = text.text.as_str().to_owned();
        if text.truncated {
            rendered.push_str(TRUNCATION_MARKER);
        }
        self.recorder.push(Call::Stream(rendered));
        Ok(message.cloned().unwrap_or_else(|| {
            let posts = self.recorder.posts.fetch_add(1, Ordering::Relaxed);
            MessageRef {
                target: target.clone(),
                id: format!("s{posts}"),
            }
        }))
    }

    async fn finalize(
        &self,
        _message: &MessageRef,
        reply: &OutboundReply,
    ) -> Result<(), TransportError> {
        if self.recorder.counted(&self.recorder.refuse_finalize) {
            return Err(TransportError::Response);
        }
        self.recorder.push(Call::StreamFinalize(reply.text.clone()));
        Ok(())
    }
}

struct TestDriver {
    recorder: Arc<Recorder>,
    surfaces: Surfaces,
    offers: Offers,
}

#[async_trait]
impl ChatDriver for TestDriver {
    async fn reply(
        &self,
        _target: &ReplyTarget,
        reply: OutboundReply,
    ) -> Result<(), TransportError> {
        self.recorder.push(Call::Reply(reply.text));
        Ok(())
    }

    fn typing(&self) -> Option<&dyn TypingLease> {
        self.offers
            .typing
            .then_some(&self.surfaces as &dyn TypingLease)
    }

    fn status(&self) -> Option<&dyn NativeStatus> {
        self.offers
            .status
            .then_some(&self.surfaces as &dyn NativeStatus)
    }

    fn progress(&self) -> Option<&dyn ProgressMessage> {
        self.offers
            .progress
            .then_some(&self.surfaces as &dyn ProgressMessage)
    }

    fn stream(&self) -> Option<&dyn TextStream> {
        self.offers
            .stream
            .then_some(&self.surfaces as &dyn TextStream)
    }

    fn reaction(&self) -> Option<&dyn InboundReaction> {
        self.offers
            .reaction
            .then_some(&self.surfaces as &dyn InboundReaction)
    }

    fn cancel_button(&self) -> Option<&dyn CancelButton> {
        self.offers
            .cancel_button
            .then_some(&self.surfaces as &dyn CancelButton)
    }
}

fn templates() -> Templates {
    Templates::resolve(&TemplateOverrides::default(), STOPPED_REPLY, FAILURE_REPLY).0
}

/// Liveness whose keep-alive is an hour out, so a test about something else never races a tick.
fn liveness(stream: bool) -> Arc<ResolvedLiveness> {
    liveness_with(
        stream,
        KeepAlive {
            at: vec![Duration::from_secs(3_600)],
            every: Duration::from_secs(3_600),
            max: 10,
        },
    )
}

/// Liveness on the shipped schedule: 15 s, 45 s, then every 60 s, ten times.
fn ticking() -> Arc<ResolvedLiveness> {
    liveness_with(false, KeepAlive::default())
}

fn liveness_with(stream: bool, keep_alive: KeepAlive) -> Arc<ResolvedLiveness> {
    Arc::new(ResolvedLiveness {
        settings: LivenessSettings {
            mode: LivenessMode::Native,
            classic_fallback: crate::config::SlackLivenessFallback::None,
            progress: ProgressSurface::Message,
            stream,
            cancel_button: false,
        },
        keep_alive,
        templates: templates(),
    })
}

/// A started policy, its recorder, its sink, and the cancellation the session shares with it.
struct Harness {
    policy: ProgressPolicy,
    sink: Arc<ProgressAdapter>,
    recorder: Arc<Recorder>,
    cancellation: SessionCancellation,
}

fn start(offers: Offers, detail: ProgressDetail, liveness: Arc<ResolvedLiveness>) -> Harness {
    start_with(offers, detail, liveness, Duration::from_secs(3), None)
}

fn start_with(
    offers: Offers,
    detail: ProgressDetail,
    liveness: Arc<ResolvedLiveness>,
    min_edit_interval: Duration,
    max_duration: Option<Duration>,
) -> Harness {
    let recorder = Arc::new(Recorder::default());
    let driver = Arc::new(TestDriver {
        recorder: Arc::clone(&recorder),
        surfaces: Surfaces {
            recorder: Arc::clone(&recorder),
            min_edit_interval,
            min_stream_interval: Duration::from_secs(2),
        },
        offers,
    });
    let cancellation = SessionCancellation::new();
    let (policy, sink) = ProgressPolicy::start(ProgressInputs {
        driver,
        target: Some(LivenessTarget::Local { connection: 1 }),
        reply: ReplyTarget::Local { connection: 1 },
        transport: "local".to_owned(),
        detail,
        liveness,
        cancellation: cancellation.clone(),
        max_duration,
    });
    Harness {
        policy,
        sink,
        recorder,
        cancellation,
    }
}

/// Lets the policy task run every step it can take without moving the clock.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

async fn advance(duration: Duration) {
    tokio::time::advance(duration).await;
    settle().await;
}

/// One turn's model text, replayed from a recorded transcript.
///
/// The only route to real [`ModelText`] outside the model client, which is what keeps a test from
/// inventing text a backend never sent.
fn recorded_delta() -> ModelText {
    let events = dekopon_model::events_from_transcript(
        dekopon_test_support::OPENAI_CHAT_COMPLETIONS_TWO_DELTAS,
    )
    .expect("the recorded transcript parses");
    dekopon_test_support::scripted_text(&events)
}

fn started() -> ProgressEvent {
    ProgressEvent::Started {
        agent: "tester".to_owned(),
        max_steps: 8,
    }
}

/// A turn that drove a capability call, which is one of the four things that may post the
/// surface. `ToolStarted` carries a `CommandWord`, which only the broker leg may construct, so the
/// tool-word rendering is pinned directly on the templates instead.
fn answered_with_tool(turn: u32) -> ProgressEvent {
    ProgressEvent::Answered {
        turn,
        tool_calls: 1,
        duration: Duration::from_millis(400),
        first_delta: None,
    }
}

/// The loop's own report that it produced an answer, which is the instant a stop is too late.
fn finished() -> ProgressEvent {
    ProgressEvent::Finished {
        outcome: SessionOutcome::Answered,
        elapsed: Duration::from_millis(900),
        turns: 1,
        tool_calls: 1,
    }
}

/// The one message a fast answer must not leave behind: a single turn with no tool call posts
/// nothing, and the answer is an ordinary reply.
#[tokio::test(start_paused = true)]
async fn a_one_turn_answer_posts_no_progress_message() {
    let mut harness = start(Offers::default(), ProgressDetail::Plain, liveness(false));
    harness.sink.emit(started());
    settle().await;
    harness
        .sink
        .emit(ProgressEvent::ModelTurn { turn: 1, of: 8 });
    harness.sink.emit(ProgressEvent::Answered {
        turn: 1,
        tool_calls: 0,
        duration: Duration::from_millis(400),
        first_delta: None,
    });
    settle().await;

    assert!(
        !harness
            .recorder
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Post(_))),
        "a fast one-turn answer must not post a message the reply immediately obsoletes: {:?}",
        harness.recorder.calls()
    );

    let delivered = harness
        .policy
        .terminal(Terminal::Answered(OutboundReply::text("done")))
        .await;
    assert!(delivered);
    assert!(
        harness
            .recorder
            .calls()
            .contains(&Call::Reply("done".to_owned())),
        "with no surface to finalize, the answer is a plain reply: {:?}",
        harness.recorder.calls()
    );
}

/// The t=0 ladder, and the first post trigger: a capability call is news worth a message.
#[tokio::test(start_paused = true)]
async fn the_ladder_runs_at_started_and_a_tool_call_posts_the_surface() {
    let mut harness = start(Offers::default(), ProgressDetail::Plain, liveness(false));
    harness.sink.emit(started());
    settle().await;
    assert_eq!(
        harness.recorder.calls(),
        vec![
            Call::Reaction(true),
            Call::Typing,
            Call::Status(Status::Working)
        ],
        "the cheapest signal first, then the lease, then the durable state"
    );

    harness.sink.emit(answered_with_tool(1));
    settle().await;
    assert_eq!(
        harness.recorder.texts(),
        vec!["Working on it…".to_owned()],
        "a turn that drove a capability call posts the surface"
    );

    harness.policy.seal();
    let delivered = harness
        .policy
        .terminal(Terminal::Answered(OutboundReply::text("a picture")))
        .await;
    assert!(delivered);
    assert_eq!(
        harness.recorder.writes().last(),
        Some(&Call::Finalize("a picture".to_owned())),
        "the surface becomes the answer in place: {:?}",
        harness.recorder.calls()
    );
    assert_eq!(
        harness.recorder.posts.load(Ordering::Relaxed),
        1,
        "one MessageRef per session"
    );
}

/// The other post trigger, on the common route: a long turn that writes text and calls nothing.
///
/// Streaming is off by default, so the text never reaches the surface — but a model that has
/// started writing is news, and without this the person watches an unchanged conversation until
/// the first keep-alive tick fifteen seconds later. The keep-alive here is an hour out, so the
/// post can only have come from the delta.
#[tokio::test(start_paused = true)]
async fn the_first_delta_posts_the_surface_with_the_stream_off() {
    let harness = start(Offers::default(), ProgressDetail::Plain, liveness(false));
    harness.sink.emit(started());
    settle().await;

    let delta = recorded_delta();
    let mut whole = delta.clone();
    harness.sink.emit(ProgressEvent::TextDelta {
        turn: 1,
        text: delta.clone(),
        cumulative_chars: whole.as_str().chars().count(),
    });
    settle().await;
    assert_eq!(
        harness.recorder.texts(),
        vec!["Working on it…".to_owned()],
        "the first delta posts the working line, and the text itself stays off the surface: {:?}",
        harness.recorder.calls()
    );

    whole.push(&delta);
    harness.sink.emit(ProgressEvent::TextDelta {
        turn: 1,
        text: delta,
        cumulative_chars: whole.as_str().chars().count(),
    });
    advance(Duration::from_secs(5)).await;
    assert_eq!(
        harness.recorder.texts(),
        vec!["Working on it…".to_owned()],
        "later text does not edit a line that does not show it: {:?}",
        harness.recorder.calls()
    );
    assert_eq!(
        harness.recorder.posts.load(Ordering::Relaxed),
        1,
        "one MessageRef per session"
    );
}

/// Two events inside one edit window are one edit carrying the later state, not two edits.
#[tokio::test(start_paused = true)]
async fn edits_inside_the_interval_coalesce_to_the_latest_state() {
    // `detailed`, because the coalescing question is which *state* the one edit carries.
    let harness = start(Offers::default(), ProgressDetail::Detailed, liveness(false));
    harness.sink.emit(started());
    harness.sink.emit(answered_with_tool(1));
    settle().await;

    harness
        .sink
        .emit(ProgressEvent::ModelTurn { turn: 2, of: 8 });
    harness
        .sink
        .emit(ProgressEvent::ModelTurn { turn: 3, of: 8 });
    settle().await;
    assert_eq!(
        harness.recorder.texts().len(),
        1,
        "an edit inside the interval waits rather than queueing: {:?}",
        harness.recorder.texts()
    );

    advance(Duration::from_secs(3)).await;
    assert_eq!(
        harness.recorder.texts().last().map(String::as_str),
        Some("Working on it… · turn 3 of 8 · 0 of 0 calls · 3 s"),
        "the coalesced edit shows the newest state, not the one it skipped"
    );
}

/// The tick schedule, read off the line the person sees: the number is fresh by construction.
#[tokio::test(start_paused = true)]
async fn keep_alive_ticks_at_fifteen_forty_five_then_every_sixty_seconds() {
    let harness = start(Offers::default(), ProgressDetail::Plain, ticking());
    harness.sink.emit(started());
    settle().await;

    advance(Duration::from_secs(15)).await;
    advance(Duration::from_secs(30)).await;
    advance(Duration::from_secs(60)).await;
    advance(Duration::from_secs(60)).await;

    assert_eq!(
        harness.recorder.texts(),
        vec![
            "Still working (15 s)…".to_owned(),
            "Still working (45 s)…".to_owned(),
            "Still working (105 s)…".to_owned(),
            "Still working (165 s)…".to_owned(),
        ],
        "15, 45, then every 60 — and the elapsed seconds are the tick's own"
    );
    assert_eq!(
        harness.recorder.posts.load(Ordering::Relaxed),
        1,
        "a keep-alive is an edit of one message, never a second post"
    );
}

/// The keep-alive budget: ten ticks, then the surface goes quiet rather than writing forever.
#[tokio::test(start_paused = true)]
async fn the_keep_alive_budget_stops_at_ten_ticks() {
    let harness = start(Offers::default(), ProgressDetail::Plain, ticking());
    harness.sink.emit(started());
    settle().await;

    for _ in 0..20 {
        advance(Duration::from_secs(60)).await;
    }

    assert_eq!(
        harness.recorder.texts().len(),
        10,
        "ten keep-alives is the bound: {:?}",
        harness.recorder.texts()
    );
}

/// The edit budget, which is the guard against a pathological event stream rather than against a
/// rate limit — the interval and the transport's own cooldown are that.
#[tokio::test(start_paused = true)]
async fn the_edit_budget_stops_at_sixty_edits() {
    let harness = start_with(
        Offers::default(),
        ProgressDetail::Plain,
        liveness(false),
        Duration::ZERO,
        None,
    );
    harness.sink.emit(started());
    harness.sink.emit(answered_with_tool(1));
    settle().await;

    for turn in 0..200 {
        harness.sink.emit(ProgressEvent::ModelTurn { turn, of: 8 });
        settle().await;
    }

    assert_eq!(
        harness.recorder.texts().len(),
        60,
        "sixty renders, including the post that opened the surface"
    );
}

/// With a stream available the stream is the surface: no progress message is posted, and the one
/// message it does create is the one that gets finalized.
#[tokio::test(start_paused = true)]
async fn a_stream_is_the_surface_and_no_progress_message_is_posted() {
    let offers = Offers {
        stream: true,
        ..Offers::default()
    };
    let mut harness = start(offers, ProgressDetail::Plain, liveness(true));
    harness.sink.emit(started());
    harness.sink.emit(answered_with_tool(1));
    settle().await;
    assert!(
        harness.recorder.texts().is_empty(),
        "a capability call must not post a second message beside the stream: {:?}",
        harness.recorder.calls()
    );

    // Only `dekopon-model` can build model text from bytes, which is what keeps anything else out
    // of a streamed surface; the empty value is enough to pin which surface renders it.
    harness.sink.emit(ProgressEvent::TextDelta {
        turn: 1,
        text: ModelText::default(),
        cumulative_chars: 0,
    });
    settle().await;
    assert_eq!(
        harness.recorder.texts(),
        vec![String::new()],
        "the stream is the one surface: {:?}",
        harness.recorder.calls()
    );

    harness.policy.seal();
    let delivered = harness
        .policy
        .terminal(Terminal::Answered(OutboundReply::text("hello there")))
        .await;
    assert!(delivered);
    assert_eq!(
        harness.recorder.writes().last(),
        Some(&Call::StreamFinalize("hello there".to_owned())),
        "the stream becomes the answer in place: {:?}",
        harness.recorder.calls()
    );
}

/// Past the surface's ceiling the stream shows a bounded prefix and says it was cut.
///
/// The policy owns the cut and the flag; the driver owns the marker. Both halves are asserted here
/// because a cut without the flag reads as a finished answer that happens to stop mid-word, and
/// there is no other signal a person could use to tell the two apart.
#[tokio::test(start_paused = true)]
async fn a_stream_past_the_surface_ceiling_is_cut_and_marked() {
    let offers = Offers {
        stream: true,
        ..Offers::default()
    };
    let harness = start(offers, ProgressDetail::Plain, liveness(true));
    harness.sink.emit(started());

    // Two fragments of one recorded turn, which together run past the ceiling above.
    let delta = recorded_delta();
    let mut whole = ModelText::default();
    for _ in 0..2 {
        whole.push(&delta);
        harness.sink.emit(ProgressEvent::TextDelta {
            turn: 1,
            text: delta.clone(),
            cumulative_chars: whole.as_str().chars().count(),
        });
    }
    settle().await;
    assert!(
        whole.as_str().chars().count() > STREAM_CEILING,
        "the fixture has to overflow the ceiling for this test to mean anything"
    );

    assert_eq!(
        harness.recorder.texts(),
        vec![format!(
            "{}{TRUNCATION_MARKER}",
            whole.truncated(STREAM_CEILING).as_str()
        )],
        "the ceiling cuts the text and `truncated` is the only thing that says so: {:?}",
        harness.recorder.calls()
    );
}

/// `finalize` failing is not a delivery failure to report: the message is removed and the answer
/// is posted the ordinary way.
#[tokio::test(start_paused = true)]
async fn a_refused_finalize_deletes_the_surface_and_replies() {
    let mut harness = start(Offers::default(), ProgressDetail::Plain, liveness(false));
    harness.recorder.refuse_finalize.store(1, Ordering::Relaxed);
    harness.sink.emit(started());
    harness.sink.emit(answered_with_tool(1));
    settle().await;

    let delivered = harness
        .policy
        .terminal(Terminal::Answered(OutboundReply::text("the answer")))
        .await;
    assert!(delivered, "the fallback is what delivered it");
    let writes = harness.recorder.writes();
    assert_eq!(
        writes[writes.len() - 2..].to_vec(),
        vec![Call::Delete, Call::Reply("the answer".to_owned())],
        "delete then reply, in that order: {:?}",
        harness.recorder.calls()
    );
}

/// Two consecutive refusals stop that surface for the session, so a message that cannot be edited
/// does not become one failed call per event for the rest of the run.
#[tokio::test(start_paused = true)]
async fn two_consecutive_failures_stop_the_surface_for_the_session() {
    let harness = start_with(
        Offers::default(),
        ProgressDetail::Plain,
        liveness(false),
        Duration::ZERO,
        None,
    );
    harness.recorder.refuse_progress.store(3, Ordering::Relaxed);
    harness.sink.emit(started());
    for turn in 1..=3 {
        harness.sink.emit(answered_with_tool(turn));
        settle().await;
    }

    assert!(
        harness.recorder.texts().is_empty(),
        "two refusals trip the rung, so the third call is never made: {:?}",
        harness.recorder.calls()
    );
    assert_eq!(
        harness.recorder.refuse_progress.load(Ordering::Relaxed),
        1,
        "one injected refusal was left unspent, which is the call that was not attempted"
    );
}

/// A stop changes the screen while the session is still parked in a model request; the ending is
/// written once, by the task that owns the message.
#[tokio::test(start_paused = true)]
async fn a_cancel_renders_the_stopped_line_before_the_session_unwinds() {
    let mut harness = start(Offers::default(), ProgressDetail::Plain, liveness(false));
    harness.sink.emit(started());
    harness.sink.emit(answered_with_tool(1));
    settle().await;

    assert!(harness.cancellation.cancel(CancelSource::User {
        via: CancelVia::StopReply
    }));
    settle().await;
    assert_eq!(
        harness
            .recorder
            .calls()
            .iter()
            .filter(|call| matches!(call, Call::Finalize(_)))
            .collect::<Vec<_>>(),
        vec![&Call::Finalize(STOPPED_REPLY.to_owned())],
        "the surface became the stopped line without waiting for the session: {:?}",
        harness.recorder.calls()
    );

    // The session reaches its own cancelled branch later and hands the terminal over; the ending
    // is already written, so nothing is said twice.
    let delivered = harness
        .policy
        .terminal(Terminal::Cancelled {
            by: CancelSource::User {
                via: CancelVia::StopReply,
            },
        })
        .await;
    assert!(!delivered, "the ending had already been written");
    assert_eq!(
        harness
            .recorder
            .calls()
            .iter()
            .filter(|call| matches!(call, Call::Finalize(_) | Call::Reply(_)))
            .count(),
        1,
        "exactly one terminal write: {:?}",
        harness.recorder.calls()
    );
}

/// A stop that arrives after the loop answered loses the race it is too late for.
///
/// The window is between the prompt loop reporting `Finished` and the session resuming, on another
/// thread, to deliver what it produced. A stop word landing inside it used to win: the policy wrote
/// the stopped line onto the surface the answer was already streamed on, and the person read their
/// answer with `Stopped.` under it. The claim the sink makes on `Finished` closes it, so the ending
/// is the answer and there is still exactly one of them.
#[tokio::test(start_paused = true)]
async fn a_stop_after_the_loop_finished_leaves_the_answer_as_the_only_terminal_write() {
    const ANSWER: &str = "The capability echoed hi.";

    let mut harness = start(Offers::default(), ProgressDetail::Plain, liveness(false));
    harness.sink.emit(started());
    harness.sink.emit(answered_with_tool(1));
    settle().await;
    harness.sink.emit(finished());

    assert!(
        !harness.cancellation.cancel(CancelSource::User {
            via: CancelVia::StopReply
        }),
        "the answer already exists, so a stop word has nothing left to stop"
    );
    settle().await;
    assert!(
        !harness
            .recorder
            .writes()
            .iter()
            .any(|call| matches!(call, Call::Finalize(_) | Call::Reply(_))),
        "the refused stop wrote no ending of its own: {:?}",
        harness.recorder.calls()
    );

    let delivered = harness
        .policy
        .terminal(Terminal::Answered(OutboundReply::text(ANSWER.to_owned())))
        .await;
    assert!(delivered, "the session's answer still reaches the person");
    assert_eq!(
        harness
            .recorder
            .writes()
            .into_iter()
            .filter(|call| matches!(call, Call::Finalize(_) | Call::Reply(_)))
            .collect::<Vec<_>>(),
        vec![Call::Finalize(ANSWER.to_owned())],
        "one terminal write, and it is the answer rather than a stopped line: {:?}",
        harness.recorder.calls()
    );
}

/// The wall-clock budget cancels the session it belongs to, counted from `Started`.
#[tokio::test(start_paused = true)]
async fn the_wall_clock_budget_cancels_the_session_from_started() {
    let harness = start_with(
        Offers::default(),
        ProgressDetail::Plain,
        liveness(false),
        Duration::from_secs(3),
        Some(Duration::from_secs(30)),
    );
    harness.sink.emit(started());
    settle().await;

    advance(Duration::from_secs(20)).await;
    assert!(
        harness.cancellation.source().is_none(),
        "the budget must not fire before it is spent"
    );

    advance(Duration::from_secs(11)).await;
    assert_eq!(
        harness.cancellation.source(),
        Some(CancelSource::Budget {
            limit: BudgetLimit::WallClock
        }),
        "the wall-clock budget cancels with its own origin"
    );
}

/// A failed session's surface says something that is no longer true; it is removed and the fixed
/// sentence is the whole reply.
#[tokio::test(start_paused = true)]
async fn a_failed_session_removes_the_surface_and_replies_the_fixed_line() {
    let mut harness = start(Offers::default(), ProgressDetail::Plain, liveness(false));
    harness.sink.emit(started());
    harness.sink.emit(answered_with_tool(1));
    settle().await;

    let delivered = harness
        .policy
        .terminal(Terminal::Failed(FAILURE_REPLY.to_owned()))
        .await;
    assert!(delivered);
    let writes = harness.recorder.writes();
    assert_eq!(
        writes[writes.len() - 2..].to_vec(),
        vec![Call::Delete, Call::Reply(FAILURE_REPLY.to_owned())],
        "the surface is removed before the sentence replaces it: {:?}",
        harness.recorder.calls()
    );
}

/// `off` keeps the service's own indicators and writes nothing.
#[tokio::test(start_paused = true)]
async fn detail_off_renders_typing_status_and_reaction_and_nothing_else() {
    let harness = start(Offers::default(), ProgressDetail::Off, ticking());
    harness.sink.emit(started());
    harness.sink.emit(answered_with_tool(1));
    settle().await;
    advance(Duration::from_secs(60)).await;

    assert!(
        harness.recorder.texts().is_empty(),
        "off posts and edits nothing: {:?}",
        harness.recorder.calls()
    );
    assert!(
        harness.recorder.calls().contains(&Call::Reaction(true))
            && harness.recorder.calls().contains(&Call::Typing)
            && harness
                .recorder
                .calls()
                .contains(&Call::Status(Status::Working)),
        "the service's own indicators still run: {:?}",
        harness.recorder.calls()
    );
}

/// `detailed` adds the counters the operator asked for; `plain` never shows them.
#[test]
fn detail_levels_differ_only_by_the_counters_they_append() {
    let templates = templates();
    let mut state = RenderState {
        turn: 2,
        of: 8,
        calls: 3,
        calls_max: 16,
        elapsed: Duration::from_secs(41),
        word: None,
    };
    state.set_word("gpt-image");

    assert_eq!(
        templates.tool(ProgressDetail::Plain, &state).as_str(),
        "Running gpt-image…"
    );
    assert_eq!(
        templates.tool(ProgressDetail::Detailed, &state).as_str(),
        "Running gpt-image… · turn 2 of 8 · 3 of 16 calls · 41 s"
    );
}

/// A provider-authored command word is bounded before it reaches a chat line.
#[test]
fn a_long_command_word_is_shortened_before_it_is_rendered() {
    let mut state = RenderState::default();
    state.set_word(&"x".repeat(100));
    let rendered = templates().tool(ProgressDetail::Plain, &state);
    assert_eq!(
        rendered.as_str(),
        format!("Running {}……", "x".repeat(32)),
        "thirty-two characters and a marker, never the whole word"
    );
}

/// Every unrenderable placeholder in the block is reported, not the first one.
#[test]
fn every_unrenderable_template_placeholder_is_reported_together() {
    let overrides = TemplateOverrides {
        working: Some("Busy with {word}".to_owned()),
        tool: Some("Running {word} for {someone}".to_owned()),
        keep_alive: None,
        stopped: Some("Stopped after {elapsed_s} s".to_owned()),
        failed: None,
    };
    let (_, problems) = Templates::resolve(&overrides, STOPPED_REPLY, FAILURE_REPLY);
    let reported: Vec<(TemplateField, String)> = problems
        .into_iter()
        .map(|problem| (problem.field, problem.placeholder))
        .collect();
    assert_eq!(
        reported,
        vec![
            (TemplateField::Working, "word".to_owned()),
            (TemplateField::Tool, "someone".to_owned()),
            (TemplateField::Stopped, "elapsed_s".to_owned()),
        ],
        "three mistakes are three refusals in one pass"
    );
}
