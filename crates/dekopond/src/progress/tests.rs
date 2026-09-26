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
        cancel_label,
        policy::CALL_DEADLINE,
        text::{RenderState, TemplateField},
    },
    session::{FAILURE_REPLY, STOPPED_REPLY, SessionCancellation},
    transport::{
        CancelButton, CancelPress, ChatDriver, InboundReaction, LivenessTarget, MessageRef,
        NativeStatus, OutboundReply, ProgressLimits, ProgressMessage, ReplyTarget, Status,
        StreamLimits, StreamedText, TextStream, TransportError, TypingLease,
    },
};

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

const STALL: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
struct Recorder {
    calls: Mutex<Vec<Call>>,
    refuse_progress: AtomicU32,
    refuse_finalize: AtomicU32,
    stall_progress: AtomicU32,
    stall_finalize: AtomicU32,
    post_attempts: AtomicU32,
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

    fn counted(&self, counter: &AtomicU32) -> bool {
        counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }

    async fn stalled(&self, counter: &AtomicU32) {
        if self.counted(counter) {
            tokio::time::sleep(STALL).await;
        }
    }

    fn writes(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| !matches!(call, Call::Reaction(_) | Call::Typing | Call::Status(_)))
            .collect()
    }
}

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
        self.recorder.post_attempts.fetch_add(1, Ordering::Relaxed);
        self.recorder.stalled(&self.recorder.stall_progress).await;
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
        self.recorder.stalled(&self.recorder.stall_progress).await;
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
        self.recorder.stalled(&self.recorder.stall_finalize).await;
        if self.recorder.counted(&self.recorder.refuse_finalize) {
            return Err(TransportError::Response);
        }
        self.recorder.push(Call::Finalize(reply.text.clone()));
        Ok(())
    }
}

const STREAM_CEILING: usize = 16;

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
        conversations: std::collections::BTreeMap::new(),
    })
}

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
        settings: liveness.settings,
        keep_alive: liveness.keep_alive.clone(),
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

fn capture() -> (
    dekopon_test_support::CaptureLayer,
    tracing::subscriber::DefaultGuard,
) {
    use tracing_subscriber::prelude::*;

    let capture = dekopon_test_support::CaptureLayer::workspace();
    let guard = tracing_subscriber::registry()
        .with(capture.clone())
        .set_default();
    (capture, guard)
}

fn events_named(capture: &dekopon_test_support::CaptureLayer, event: &str) -> Vec<String> {
    let rendered = format!("event=\"{event}\"");
    capture
        .events()
        .into_iter()
        .map(|(fields, _)| fields)
        .filter(|fields| fields.contains(&rendered))
        .collect()
}

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

async fn advance(duration: Duration) {
    tokio::time::advance(duration).await;
    settle().await;
}

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

fn answered_with_tool(turn: u32) -> ProgressEvent {
    ProgressEvent::Answered {
        turn,
        tool_calls: 1,
        duration: Duration::from_millis(400),
        first_delta: None,
    }
}

fn finished() -> ProgressEvent {
    ProgressEvent::Finished {
        outcome: SessionOutcome::Answered,
        elapsed: Duration::from_millis(900),
        turns: 1,
        tool_calls: 1,
    }
}

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

#[tokio::test(start_paused = true)]
async fn edits_inside_the_interval_coalesce_to_the_latest_state() {
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

    let text = recorded_delta();
    harness.sink.emit(ProgressEvent::TextDelta {
        turn: 1,
        cumulative_chars: text.as_str().chars().count(),
        text: text.clone(),
    });
    settle().await;
    assert_eq!(
        harness.recorder.texts(),
        vec![text.as_str().to_owned()],
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

#[tokio::test(start_paused = true)]
async fn auto_selects_existing_indicators_and_only_falls_back_to_supported_messages() {
    for (name, status, typing, reaction, progress, expected) in [
        (
            "slack-agent",
            true,
            false,
            false,
            true,
            Some(Call::Status(Status::Working)),
        ),
        (
            "slack-classic",
            false,
            false,
            true,
            true,
            Some(Call::Reaction(true)),
        ),
        (
            "telegram-discord",
            false,
            true,
            true,
            true,
            Some(Call::Typing),
        ),
        ("whatsapp", false, true, false, false, Some(Call::Typing)),
        ("editable-only", false, false, false, true, None),
        ("reply-only", false, false, false, false, None),
    ] {
        let mut config = ticking();
        Arc::get_mut(&mut config)
            .expect("unshared fixture")
            .settings
            .progress = ProgressSurface::Auto;
        let mut harness = start(
            Offers {
                status,
                typing,
                reaction,
                progress,
                stream: false,
                cancel_button: false,
            },
            ProgressDetail::Plain,
            config,
        );
        harness.sink.emit(started());
        settle().await;
        assert_eq!(
            harness.recorder.calls(),
            expected.clone().into_iter().collect::<Vec<_>>(),
            "{name}"
        );
        for turn in 1..=4 {
            harness.sink.emit(answered_with_tool(turn));
        }
        advance(Duration::from_secs(16)).await;
        let message_expected = expected.is_none() && progress;
        assert_eq!(
            !harness.recorder.texts().is_empty(),
            message_expected,
            "{name}"
        );
        assert!(
            harness
                .policy
                .terminal(Terminal::Answered(OutboundReply::text("done")))
                .await
        );
        settle().await;
        assert_eq!(
            harness.recorder.writes().last(),
            Some(&if message_expected {
                Call::Finalize("done".to_owned())
            } else {
                Call::Reply("done".to_owned())
            }),
            "{name}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn answer_streaming_is_independent_of_progress_and_detail_off() {
    for progress in [
        ProgressSurface::Auto,
        ProgressSurface::Off,
        ProgressSurface::Message,
    ] {
        let mut config = liveness(true);
        Arc::get_mut(&mut config)
            .expect("unshared fixture")
            .settings
            .progress = progress;
        let mut harness = start(
            Offers {
                stream: true,
                ..Offers::default()
            },
            ProgressDetail::Off,
            config,
        );
        harness.sink.emit(started());
        harness.sink.emit(answered_with_tool(1));
        settle().await;
        assert!(harness.recorder.writes().is_empty());
        let text = recorded_delta();
        harness.sink.emit(ProgressEvent::TextDelta {
            turn: 1,
            cumulative_chars: text.as_str().chars().count(),
            text: text.clone(),
        });
        settle().await;
        assert_eq!(
            harness.recorder.writes(),
            vec![Call::Stream(text.as_str().to_owned())]
        );
        assert!(
            harness
                .policy
                .terminal(Terminal::Answered(OutboundReply::text("done")))
                .await
        );
        assert_eq!(
            harness.recorder.writes().last(),
            Some(&Call::StreamFinalize("done".to_owned()))
        );
    }
}

#[tokio::test(start_paused = true)]
async fn steering_clears_pending_text_and_restarts_the_same_numbered_turn() {
    let mut harness = start(
        Offers {
            stream: true,
            ..Offers::default()
        },
        ProgressDetail::Off,
        liveness(true),
    );
    harness.sink.emit(started());
    let text = recorded_delta();
    let delta = || ProgressEvent::TextDelta {
        turn: 1,
        cumulative_chars: text.as_str().chars().count(),
        text: text.clone(),
    };
    harness.sink.emit(delta());
    settle().await;
    harness.sink.emit(delta());
    settle().await;
    harness.sink.emit(ProgressEvent::Steered { turn: 1 });
    settle().await;
    advance(Duration::from_secs(2)).await;
    assert_eq!(
        harness.recorder.writes(),
        [Call::Stream(text.as_str().to_owned())]
    );
    harness
        .sink
        .emit(ProgressEvent::ModelTurn { turn: 1, of: 1 });
    harness.sink.emit(delta());
    settle().await;
    assert_eq!(
        harness.recorder.writes(),
        [
            Call::Stream(text.as_str().to_owned()),
            Call::Stream(text.as_str().to_owned())
        ]
    );
    assert!(
        harness
            .policy
            .terminal(Terminal::Answered(OutboundReply::text("done")))
            .await
    );
}

#[tokio::test(start_paused = true)]
async fn a_silent_terminal_after_steering_never_sends_an_empty_edit() {
    let mut harness = start(
        Offers {
            stream: true,
            ..Offers::default()
        },
        ProgressDetail::Off,
        liveness(true),
    );
    harness.sink.emit(started());
    let text = recorded_delta();
    harness.sink.emit(ProgressEvent::TextDelta {
        turn: 1,
        cumulative_chars: text.as_str().chars().count(),
        text: text.clone(),
    });
    settle().await;
    harness.sink.emit(ProgressEvent::Steered { turn: 1 });
    assert!(!harness.policy.terminal(Terminal::Silent).await);
    assert_eq!(
        harness.recorder.writes(),
        [Call::Stream(text.as_str().to_owned())]
    );
}

#[tokio::test(start_paused = true)]
async fn auto_preserves_explicit_buttons_and_disabled_liveness() {
    for (mode, button, expect_message) in [
        (LivenessMode::Native, true, true),
        (LivenessMode::Off, false, false),
    ] {
        let mut config = liveness(false);
        let settings = &mut Arc::get_mut(&mut config)
            .expect("unshared fixture")
            .settings;
        settings.progress = ProgressSurface::Auto;
        settings.mode = mode;
        settings.cancel_button = button;
        let mut harness = start(
            Offers {
                cancel_button: button,
                ..Offers::default()
            },
            ProgressDetail::Plain,
            config,
        );
        harness.sink.emit(started());
        harness.sink.emit(answered_with_tool(1));
        settle().await;
        assert_eq!(!harness.recorder.texts().is_empty(), expect_message);
        if mode == LivenessMode::Off {
            assert!(harness.recorder.calls().is_empty());
        }
        assert!(
            harness
                .policy
                .terminal(Terminal::Answered(OutboundReply::text("done")))
                .await
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_stream_past_the_surface_ceiling_is_cut_and_marked() {
    let offers = Offers {
        stream: true,
        ..Offers::default()
    };
    let harness = start(offers, ProgressDetail::Plain, liveness(true));
    harness.sink.emit(started());

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

#[tokio::test(start_paused = true)]
async fn two_consecutive_failures_stop_the_surface_for_the_session() {
    let (capture, _guard) = capture();
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
    let degraded = events_named(&capture, "gateway_progress_degraded");
    assert_eq!(
        degraded.len(),
        1,
        "the rung stops once and says so once: {degraded:?}"
    );
    assert!(
        degraded[0].contains("primitive=\"progress\"")
            && degraded[0].contains("category=\"response\""),
        "the record names the rung that stopped and the cause that stopped it: {degraded:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_creating_post_that_misses_its_deadline_stops_the_surface_at_the_first_failure() {
    let (capture, _guard) = capture();
    let harness = start_with(
        Offers::default(),
        ProgressDetail::Plain,
        liveness(false),
        Duration::ZERO,
        None,
    );
    harness.recorder.stall_progress.store(1, Ordering::Relaxed);
    harness.sink.emit(started());
    harness.sink.emit(answered_with_tool(1));
    settle().await;
    advance(CALL_DEADLINE + Duration::from_secs(1)).await;

    harness.sink.emit(answered_with_tool(2));
    settle().await;
    assert_eq!(
        harness.recorder.post_attempts.load(Ordering::Relaxed),
        1,
        "a second trigger posted beside a message that may already exist: {:?}",
        harness.recorder.calls()
    );
    let degraded = events_named(&capture, "gateway_progress_degraded");
    assert_eq!(
        degraded.len(),
        1,
        "one miss on a creating call is the whole budget: {degraded:?}"
    );
    assert!(
        degraded[0].contains("primitive=\"progress\"")
            && degraded[0].contains("category=\"deadline\""),
        "the record says the deadline stopped it, not a refusal: {degraded:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_finalize_that_misses_its_deadline_replies_rather_than_deleting_the_surface() {
    let mut harness = start(Offers::default(), ProgressDetail::Plain, liveness(false));
    harness.sink.emit(started());
    harness.sink.emit(answered_with_tool(1));
    settle().await;
    harness.recorder.stall_finalize.store(1, Ordering::Relaxed);

    let delivered = harness
        .policy
        .terminal(Terminal::Answered(OutboundReply::text("the answer")))
        .await;

    assert!(delivered, "the fallback reply is what delivered it");
    let writes = harness.recorder.writes();
    assert!(
        !writes.contains(&Call::Delete),
        "a finalize that may have landed must not have its message deleted: {writes:?}"
    );
    assert_eq!(
        writes.last(),
        Some(&Call::Reply("the answer".to_owned())),
        "the answer is posted beside the surface instead: {writes:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_streamed_session_closes_the_stream_under_the_partial_answer() {
    let offers = Offers {
        stream: true,
        ..Offers::default()
    };
    let mut harness = start(offers, ProgressDetail::Plain, liveness(true));
    harness.sink.emit(started());
    let delta = recorded_delta();
    harness.sink.emit(ProgressEvent::TextDelta {
        turn: 1,
        text: delta.clone(),
        cumulative_chars: delta.as_str().chars().count(),
    });
    settle().await;
    assert!(
        !harness.recorder.texts().is_empty(),
        "the stream has to be the surface for this to be about closing it: {:?}",
        harness.recorder.calls()
    );

    let delivered = harness
        .policy
        .terminal(Terminal::Failed(FAILURE_REPLY.to_owned()))
        .await;

    assert!(delivered);
    let writes = harness.recorder.writes();
    assert_eq!(
        writes.last(),
        Some(&Call::StreamFinalize(format!(
            "{}\n\n{FAILURE_REPLY}",
            delta.as_str()
        ))),
        "the stream ends on what was read with the failure under it: {writes:?}"
    );
    assert!(
        !writes.iter().any(|call| matches!(call, Call::Reply(_))),
        "the failure must not arrive as a second message beside a stream left open: {writes:?}"
    );
}

#[test]
fn a_stop_word_cancel_reports_the_affordance_that_won_the_race() {
    let cancellation = SessionCancellation::new();
    assert_eq!(
        dekopon_agent::prompt::CancellationProbe::cancel_source(&cancellation),
        None,
        "a running session has no origin yet"
    );
    assert!(cancellation.cancel(CancelSource::User {
        via: CancelVia::StopReply
    }));
    assert_eq!(
        dekopon_agent::prompt::CancellationProbe::cancel_source(&cancellation),
        Some(CancelSource::User {
            via: CancelVia::StopReply
        })
    );
    assert_eq!(
        cancel_label(CancelSource::User {
            via: CancelVia::StopReply
        }),
        "user:stop-reply"
    );

    let shutdown = SessionCancellation::new();
    assert!(shutdown.cancel(CancelSource::Budget {
        limit: BudgetLimit::WallClock
    }));
    assert!(!shutdown.cancel(CancelSource::Operator));
    assert_eq!(
        dekopon_agent::prompt::CancellationProbe::cancel_source(&shutdown),
        Some(CancelSource::Budget {
            limit: BudgetLimit::WallClock
        })
    );
}

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

#[tokio::test]
async fn local_image_answers_fall_back_once_while_text_finalizes_in_place() {
    use crate::transport::{ChatTransport, TransportEvent, local::LocalTransport};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    for stream in [false, true] {
        for with_image in [false, true] {
            use std::os::unix::fs::PermissionsExt as _;
            let directory = tempfile::tempdir().unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            let path = directory.path().join("dev.sock");
            let config = liveness(stream);
            let mut transport =
                LocalTransport::new("dev".to_owned(), path.clone(), config.settings);
            transport.connect().await.unwrap();
            let mut client = tokio::net::UnixStream::connect(path).await.unwrap();
            client
                .write_all(
                    format!(
                        "{}\n",
                        json!({"subject": "tel.16034700182", "text": "hello"})
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let TransportEvent::Message(inbound) =
                tokio::time::timeout(Duration::from_secs(2), transport.next())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("message")
            };
            let (mut policy, sink) = ProgressPolicy::start(ProgressInputs {
                driver: transport.driver(),
                target: inbound.liveness.clone(),
                reply: inbound.reply.clone(),
                transport: "dev".to_owned(),
                detail: ProgressDetail::Plain,
                settings: config.settings,
                keep_alive: config.keep_alive.clone(),
                liveness: config,
                cancellation: SessionCancellation::new(),
                max_duration: None,
            });
            sink.emit(started());
            if stream {
                let text = recorded_delta();
                sink.emit(ProgressEvent::TextDelta {
                    turn: 1,
                    cumulative_chars: text.as_str().chars().count(),
                    text,
                });
            } else {
                sink.emit(answered_with_tool(1));
            }
            let mut reader = BufReader::new(client);
            let surface = if stream { "delta" } else { "progress" };
            let mut lines = Vec::new();
            let id = loop {
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap();
                let line: Value = serde_json::from_str(&line).unwrap();
                let id = line[surface]["id"].as_str().map(str::to_owned);
                lines.push(line);
                if let Some(id) = id {
                    break id;
                }
            };
            let png = b"\x89PNG\r\n\x1a\nlocal fidelity sentinel";
            let reply = if with_image {
                OutboundReply::with_images(
                    "done",
                    vec![
                        dekopon_agent::attachment::GeneratedImage::from_png(png.to_vec()).unwrap(),
                    ],
                )
            } else {
                OutboundReply::text("done")
            };
            assert!(policy.terminal(Terminal::Answered(reply)).await);
            transport
                .driver()
                .reply(&inbound.reply, OutboundReply::text("end marker"))
                .await
                .unwrap();
            loop {
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap();
                let line: Value = serde_json::from_str(&line).unwrap();
                if line["reply"] == "end marker" {
                    break;
                }
                lines.push(line);
            }
            let answers = lines
                .iter()
                .filter(|line| line["reply"] == "done")
                .collect::<Vec<_>>();
            assert_eq!(answers.len(), 1, "{lines:?}");
            let answer = answers[0];
            let deleted = lines
                .iter()
                .position(|line| line["progress"]["deleted"] == true);
            if with_image {
                assert!(answer.get("id").is_none(), "owned fallback, not finalize");
                assert_eq!(answer["images"][0]["filename"], "asset-1.png");
                assert_eq!(answer["images"][0]["mediaType"], "image/png");
                assert_eq!(
                    STANDARD
                        .decode(answer["images"][0]["data"].as_str().unwrap())
                        .unwrap(),
                    png
                );
                if stream {
                    assert!(deleted.is_none(), "append-only stream is preserved");
                } else {
                    let deleted = deleted.expect("progress removed before fallback");
                    assert_eq!(lines[deleted]["progress"]["id"], id);
                    assert!(
                        deleted
                            < lines
                                .iter()
                                .position(|line| line["reply"] == "done")
                                .unwrap()
                    );
                }
            } else {
                assert_eq!(answer["id"], id);
                assert!(answer.get("images").is_none());
                assert!(deleted.is_none());
            }
        }
    }
}
