//! This task is the only writer of the terminal message once a session starts, since two tasks
//! editing one message would race with no ordering between them.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use dekopon_agent::{BudgetLimit, CancelSource, ProgressEvent};
use tokio::{
    sync::{Notify, mpsc, oneshot, watch},
    time::Instant,
};

use crate::{
    config::{LivenessMode, LivenessSettings, ProgressSurface, ResolvedLiveness},
    progress::{
        KeepAlive, StopCause,
        adapter::{EVENT_QUEUE, ProgressAdapter, ProgressCounters, QueuedEvent, record},
        cancel_label, stop_line,
        text::{LiveNote, ProgressDetail, ProgressText, RenderState},
    },
    session::SessionCancellation,
    transport::{
        ChatDriver, LivenessTarget, MessageRef, OutboundReply, ReplyTarget, Status, StreamedText,
        TransportError,
    },
};

const MAX_CONSECUTIVE_FAILURES: u8 = 2;
const MAX_EDITS: u32 = 60;
/// Two seconds bounds a hung endpoint rather than a slow one; this task is single-tasked, so
/// replies otherwise wait behind a typing indicator as long as the socket is held.
pub(super) const CALL_DEADLINE: Duration = Duration::from_secs(2);
const DEADLINE_MISSED: &str = "deadline";

pub(crate) async fn bounded<T>(
    call: impl std::future::Future<Output = Result<T, TransportError>>,
) -> Result<T, &'static str> {
    match tokio::time::timeout(CALL_DEADLINE, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.category()),
        Err(_elapsed) => Err(DEADLINE_MISSED),
    }
}

const RUNNING: u8 = 0;
const SEALED: u8 = 1;
const FINISHED: u8 = 2;

#[derive(Debug)]
pub(crate) enum Terminal {
    Answered(OutboundReply),
    Stopped(StopCause),
    Failed(String),
    Silent,
}

struct TerminalRequest {
    terminal: Terminal,
    done: oneshot::Sender<bool>,
}

#[derive(Default)]
struct Coordination {
    state: AtomicU8,
    changed: Notify,
}

impl Coordination {
    fn seal(&self) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "the Err payload is the current state, and every state that loses this \
                      compare_exchange is one that already stops rendering"
        )]
        let _ = self
            .state
            .compare_exchange(RUNNING, SEALED, Ordering::AcqRel, Ordering::Acquire);
        self.changed.notify_waiters();
    }

    fn finish(&self) {
        self.state.store(FINISHED, Ordering::Release);
        self.changed.notify_waiters();
    }

    fn running(&self) -> bool {
        self.state.load(Ordering::Acquire) == RUNNING
    }

    async fn finished(&self) {
        loop {
            let changed = self.changed.notified();
            if self.state.load(Ordering::Acquire) == FINISHED {
                return;
            }
            changed.await;
        }
    }
}

pub(crate) struct ProgressInputs {
    pub driver: Arc<dyn ChatDriver>,
    pub target: Option<LivenessTarget>,
    pub reply: ReplyTarget,
    pub transport: String,
    pub detail: ProgressDetail,
    pub liveness: Arc<ResolvedLiveness>,
    pub settings: LivenessSettings,
    pub keep_alive: KeepAlive,
    pub cancellation: SessionCancellation,
    pub max_duration: Option<Duration>,
}

pub(crate) struct ProgressPolicy {
    coordination: Arc<Coordination>,
    terminal: Option<oneshot::Sender<TerminalRequest>>,
}

impl ProgressPolicy {
    pub(crate) fn start(inputs: ProgressInputs) -> (Self, Arc<ProgressAdapter>) {
        let coordination = Arc::new(Coordination::default());
        let counters = Arc::new(ProgressCounters::default());
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
        let (text_tx, text_rx) = watch::channel(StreamedText::default());
        let (terminal_tx, terminal_rx) = oneshot::channel();
        let cancellation = inputs.cancellation.clone();
        let adapter = Arc::new(ProgressAdapter::new(
            inputs.transport.clone(),
            events_tx,
            text_tx,
            Arc::clone(&counters),
            cancellation.clone(),
        ));
        let surface = Surface::new(inputs, Arc::clone(&coordination), counters);
        #[expect(
            clippy::disallowed_methods,
            reason = "detached on purpose: cleanup now finishes before the terminal \
                      acknowledgment that frees the session's admission permit, but cancellation \
                      and coordination-end exits still run this after their caller stops \
                      watching; bounded by EVENT_QUEUE and each driver call's timeout, and it \
                      ends once the terminal request and event queue are done"
        )]
        drop(tokio::spawn(tracing::Instrument::instrument(
            run(surface, events_rx, text_rx, terminal_rx, cancellation),
            tracing::Span::current(),
        )));
        (
            Self {
                coordination,
                terminal: Some(terminal_tx),
            },
            adapter,
        )
    }

    pub(crate) fn seal(&self) {
        self.coordination.seal();
    }

    pub(crate) async fn terminal(&mut self, terminal: Terminal) -> bool {
        let Some(sender) = self.terminal.take() else {
            return false;
        };
        let (done, wait) = oneshot::channel();
        if sender.send(TerminalRequest { terminal, done }).is_err() {
            return false;
        }
        wait.await.unwrap_or(false)
    }
}

impl Drop for ProgressPolicy {
    fn drop(&mut self) {
        self.coordination.finish();
    }
}

#[derive(Debug, Default)]
struct Breaker {
    failures: u8,
    tripped: bool,
}

impl Breaker {
    fn allows(&self) -> bool {
        !self.tripped
    }

    fn succeeded(&mut self) {
        self.failures = 0;
    }

    fn failed(&mut self, transport: &str, primitive: &'static str, category: &str) {
        self.observed(transport, primitive, category, MAX_CONSECUTIVE_FAILURES);
    }

    /// A deadline miss, unlike an error, may mean the post still lands later, so retrying would
    /// create a second message; one miss trips this rung instead of two.
    fn orphaned(&mut self, transport: &str, primitive: &'static str) {
        self.observed(transport, primitive, DEADLINE_MISSED, 1);
    }

    fn observed(&mut self, transport: &str, primitive: &'static str, category: &str, ceiling: u8) {
        tracing::debug!(
            event = "gateway_progress_rendered",
            transport = %transport,
            primitive,
            outcome = "error",
            category
        );
        self.failures = self.failures.saturating_add(1);
        if self.failures >= ceiling && !self.tripped {
            self.tripped = true;
            tracing::debug!(
                event = "gateway_progress_degraded",
                transport = %transport,
                primitive,
                category
            );
        }
    }
}

#[derive(Debug, Default)]
struct Breakers {
    typing: Breaker,
    status: Breaker,
    status_text: Breaker,
    reaction: Breaker,
    progress: Breaker,
    stream: Breaker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Line {
    Status,
    KeepAlive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatusTextState {
    Waiting,
    HandedOver,
    RestoreAttempted,
}

// A past instant resolves without a new timer: one registered after the clock has moved fires only
// on the driver's next turn, behind every other timer that came due in the same jump.
async fn until(at: Option<Instant>) {
    match at {
        Some(at) if at <= Instant::now() => {}
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

struct Deadline(Option<Instant>);

impl Deadline {
    fn from_budget(started: Instant, budget: Option<Duration>) -> Self {
        Self(budget.map(|budget| started + budget))
    }

    async fn fired(&self) {
        until(self.0).await;
    }

    // Disarms only: once fired, a session that already claimed completion must not wake this
    // loop again before its terminal request arrives.
    fn spend(&mut self) {
        self.0 = None;
    }
}

struct Schedule {
    next: Option<Instant>,
    every: Duration,
}

impl Schedule {
    const fn idle(every: Duration) -> Self {
        Self { next: None, every }
    }

    fn arm(&mut self, from: Instant) {
        self.next = Some(from + self.every);
    }

    fn stop(&mut self) {
        self.next = None;
    }

    async fn due(&self) {
        until(self.next).await;
    }
}

struct KeepAliveTicks {
    fired: u32,
    next: Option<Instant>,
}

impl KeepAliveTicks {
    fn start(cadence: &KeepAlive, from: Instant) -> Self {
        let mut ticks = Self {
            fired: 0,
            next: None,
        };
        ticks.schedule(cadence, from);
        ticks
    }

    fn tick(&mut self, cadence: &KeepAlive, now: Instant) -> u32 {
        self.fired = self.fired.saturating_add(1);
        self.schedule(cadence, now);
        self.fired
    }

    fn schedule(&mut self, cadence: &KeepAlive, from: Instant) {
        if self.fired >= cadence.max {
            self.next = None;
            return;
        }
        let fired = self.fired as usize;
        let gap = match cadence.at.get(fired) {
            Some(offset) => {
                let previous = fired
                    .checked_sub(1)
                    .and_then(|index| cadence.at.get(index))
                    .copied()
                    .unwrap_or_default();
                offset.saturating_sub(previous)
            }
            None => cadence.every,
        };
        self.next = Some(from + gap);
    }

    const fn exhausted(&self) -> bool {
        self.next.is_none()
    }

    async fn due(&self) {
        until(self.next).await;
    }
}

struct Throttle<T> {
    pending: Option<T>,
    next: Option<Instant>,
}

impl<T> Throttle<T> {
    const fn new() -> Self {
        Self {
            pending: None,
            next: None,
        }
    }

    fn waiting(&self, now: Instant) -> bool {
        self.next.is_some_and(|next| now < next)
    }

    fn defer(&mut self, work: T) {
        self.pending = Some(work);
    }

    fn take(&mut self) -> Option<T> {
        self.pending.take()
    }

    fn hold(&mut self, from: Instant, interval: Duration) {
        self.next = Some(from + interval);
    }

    async fn due(&self) {
        if self.pending.is_some() {
            until(self.next).await;
        } else {
            std::future::pending::<()>().await;
        }
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "reshaped by the unit that next rewrites this"
)]
struct Surface {
    driver: Arc<dyn ChatDriver>,
    target: Option<LivenessTarget>,
    reply: ReplyTarget,
    transport: String,
    detail: ProgressDetail,
    liveness: Arc<ResolvedLiveness>,
    settings: LivenessSettings,
    keep_alive: KeepAlive,
    coordination: Arc<Coordination>,
    counters: Arc<ProgressCounters>,
    cancellation: SessionCancellation,
    max_duration: Option<Duration>,

    state: RenderState,
    streaming: bool,
    latest_text: Option<StreamedText>,
    posted_on_text: bool,
    edits: u32,
    budget_reported: bool,
    breakers: Breakers,
    indicator_active: bool,
    status_attempted: bool,
    status_text: StatusTextState,
    reaction_attempted: bool,
}

impl Surface {
    fn new(
        inputs: ProgressInputs,
        coordination: Arc<Coordination>,
        counters: Arc<ProgressCounters>,
    ) -> Self {
        Self {
            driver: inputs.driver,
            target: inputs.target,
            reply: inputs.reply,
            transport: inputs.transport,
            detail: inputs.detail,
            liveness: inputs.liveness,
            settings: inputs.settings,
            keep_alive: inputs.keep_alive,
            coordination,
            counters,
            cancellation: inputs.cancellation,
            max_duration: inputs.max_duration,
            state: RenderState::default(),
            streaming: false,
            latest_text: None,
            posted_on_text: false,
            edits: 0,
            budget_reported: false,
            breakers: Breakers::default(),
            indicator_active: false,
            status_attempted: false,
            status_text: StatusTextState::Waiting,
            reaction_attempted: false,
        }
    }

    fn native(&self) -> bool {
        self.settings.mode == LivenessMode::Native && self.target.is_some()
    }

    fn streams(&self) -> bool {
        self.native() && self.settings.stream && self.driver.stream().is_some()
    }

    fn cancel_control(&self) -> bool {
        self.settings.cancel_button && self.driver.cancel_button().is_some()
    }

    fn clear_obsolete_note(&mut self) -> bool {
        if self.state.note.as_ref().is_some_and(|note| {
            note.generation != self.counters.note_generation.load(Ordering::Acquire)
        }) {
            self.state.note = None;
            true
        } else {
            false
        }
    }

    async fn open_reaction(&mut self) {
        let Some(target) = self.target.clone() else {
            return;
        };
        let driver = Arc::clone(&self.driver);
        if let Some(reaction) = driver.reaction() {
            self.reaction_attempted = true;
            let outcome = bounded(reaction.set(&target, true)).await;
            self.indicator_active |= outcome.is_ok();
            self.observe(outcome, "reaction");
        }
    }

    async fn restore_native_status(&mut self) {
        if self.status_text != StatusTextState::HandedOver {
            return;
        }
        self.status_text = StatusTextState::RestoreAttempted;
        let driver = Arc::clone(&self.driver);
        if let (Some(target), Some(status)) = (&self.target, driver.status()) {
            self.status_attempted = true;
            let outcome = bounded(status.set(target, Status::Working)).await;
            self.observe(outcome, "status");
        }
    }

    async fn clear_status_text(&mut self) {
        if self.status_text == StatusTextState::Waiting {
            return;
        }
        self.status_text = StatusTextState::Waiting;
        let driver = Arc::clone(&self.driver);
        if let (Some(target), Some(status_text)) = (&self.target, driver.status_text()) {
            let outcome = bounded(status_text.clear(target)).await;
            self.observe(outcome, "status_text");
        }
    }

    fn line(&self, line: Line) -> ProgressText {
        let templates = &self.liveness.templates;
        if let Some(note) = &self.state.note {
            return if note.eta.is_some() {
                templates.note_eta(self.detail, &self.state)
            } else {
                templates.note(self.detail, &self.state)
            };
        }
        match line {
            Line::KeepAlive => templates.keep_alive(self.detail, &self.state),
            Line::Status if self.state.word.is_some() => templates.tool(self.detail, &self.state),
            Line::Status => templates.working(self.detail, &self.state),
        }
    }

    async fn end(
        &mut self,
        terminal: Terminal,
        text: StreamedText,
        message: Option<MessageRef>,
        keep_alives: u32,
    ) -> bool {
        self.state.note = None;
        let generation = text.generation;
        self.latest_text = (!text.text.as_str().is_empty()).then_some(text);
        self.coordination.seal();
        self.clear_status_text().await;
        self.record_terminal(&terminal, keep_alives);
        let streamed = self.streamed_text();
        match terminal {
            Terminal::Answered(reply) => self.finalize(message, reply, generation).await,
            Terminal::Stopped(cause @ StopCause::Cancelled(_)) => {
                let stopped = stop_line(cause, &self.liveness.templates);
                let reply = match streamed {
                    Some(partial) => ended(&partial, &stopped),
                    None => stopped,
                };
                self.finalize(message, OutboundReply::text(reply), generation)
                    .await
            }
            Terminal::Stopped(
                cause @ (StopCause::Model(_)
                | StopCause::EmptyAnswer
                | StopCause::MaxSteps
                | StopCause::SessionTask),
            ) => {
                let line = stop_line(cause, &self.liveness.templates);
                self.fail(message, line, streamed, generation).await
            }
            Terminal::Failed(line) => self.fail(message, line, streamed, generation).await,
            Terminal::Silent => {
                match streamed.filter(|partial| !partial.is_empty()) {
                    Some(partial) => {
                        self.finalize(message, OutboundReply::text(partial), generation)
                            .await;
                    }
                    None => self.discard(message).await,
                }
                false
            }
        }
    }

    async fn fail(
        &mut self,
        message: Option<MessageRef>,
        line: String,
        streamed: Option<String>,
        generation: u64,
    ) -> bool {
        match streamed {
            Some(partial) => {
                self.finalize(
                    message,
                    OutboundReply::text(ended(&partial, &line)),
                    generation,
                )
                .await
            }
            None => {
                self.discard(message).await;
                self.deliver(OutboundReply::text(line)).await
            }
        }
    }

    fn streamed_text(&self) -> Option<String> {
        self.streaming.then(|| {
            self.latest_text
                .as_ref()
                .map(|text| text.text.as_str())
                .unwrap_or_default()
                .to_owned()
        })
    }

    async fn finalize(
        &mut self,
        message: Option<MessageRef>,
        reply: OutboundReply,
        generation: u64,
    ) -> bool {
        if let Some(message) = message {
            let driver = Arc::clone(&self.driver);
            let finalized = if self.streaming {
                match driver.stream() {
                    Some(stream) => {
                        Some(bounded(stream.finalize(&message, &reply, generation)).await)
                    }
                    None => None,
                }
            } else {
                match driver.progress() {
                    Some(progress) => Some(bounded(progress.finalize(&message, &reply)).await),
                    None => None,
                }
            };
            match finalized {
                Some(Ok(())) => {
                    tracing::debug!(
                        event = "gateway_progress_rendered",
                        transport = %self.transport,
                        primitive = "finalize",
                        outcome = "ok"
                    );
                    return true;
                }
                Some(Err(category)) => {
                    tracing::debug!(
                        event = "gateway_progress_rendered",
                        transport = %self.transport,
                        primitive = "finalize",
                        outcome = "error",
                        category
                    );
                    if category == DEADLINE_MISSED {
                        return self.deliver(reply).await;
                    }
                }
                None => {}
            }
            self.discard(Some(message)).await;
        }
        self.deliver(reply).await
    }

    async fn discard(&mut self, message: Option<MessageRef>) {
        let Some(message) = message else {
            return;
        };
        let driver = Arc::clone(&self.driver);
        if self.streaming {
            if let Some(stream) = driver.stream() {
                let outcome = bounded(stream.discard(&message)).await;
                self.observe(outcome, "stream-discard");
            }
            return;
        }
        let Some(progress) = driver.progress() else {
            return;
        };
        let outcome = bounded(progress.delete(&message)).await;
        self.observe(outcome, "delete");
    }

    async fn deliver(&mut self, reply: OutboundReply) -> bool {
        let driver = Arc::clone(&self.driver);
        match driver.reply(&self.reply, reply).await {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(event = "gateway_reply_failed", category = error.category());
                false
            }
        }
    }

    fn record_terminal(&self, terminal: &Terminal, keep_alives: u32) {
        tracing::info!(
            target: "dekopon_gatewayd::audit",
            {
                audit.event = "gateway.progress",
                kind = match terminal {
                    Terminal::Answered(_) => "terminal_answered",
                    Terminal::Stopped(StopCause::Cancelled(_)) => "terminal_cancelled",
                    Terminal::Stopped(
                        StopCause::Model(_)
                        | StopCause::EmptyAnswer
                        | StopCause::MaxSteps
                        | StopCause::SessionTask,
                    )
                    | Terminal::Failed(_) => "terminal_failed",
                    Terminal::Silent => "terminal_silent",
                },
                by = match terminal {
                    Terminal::Stopped(StopCause::Cancelled(by)) => Some(cancel_label(*by)),
                    Terminal::Stopped(
                        StopCause::Model(_)
                        | StopCause::EmptyAnswer
                        | StopCause::MaxSteps
                        | StopCause::SessionTask,
                    )
                    | Terminal::Answered(_)
                    | Terminal::Failed(_)
                    | Terminal::Silent => None,
                },
                edits = self.edits,
                keep_alives,
                stream.deltas = self.counters.deltas.load(Ordering::Relaxed),
                progress.dropped = self.counters.dropped.load(Ordering::Relaxed),
                progress.notes_dropped = self.counters.notes_dropped.load(Ordering::Relaxed),
            },
            "gateway progress"
        );
    }

    async fn cleanup(&mut self, message: Option<MessageRef>) {
        self.state.note = None;
        self.clear_status_text().await;
        if !self.streaming {
            self.discard(message).await;
        }
        if !self.native() {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let driver = Arc::clone(&self.driver);
        if self.status_attempted
            && let Some(status) = driver.status()
        {
            let outcome = bounded(status.set(&target, Status::Idle)).await;
            self.observe(outcome, "status");
        }
        if self.reaction_attempted
            && let Some(reaction) = driver.reaction()
        {
            let outcome = bounded(reaction.set(&target, false)).await;
            self.observe(outcome, "reaction");
        }
    }

    fn observe(&mut self, outcome: Result<(), &'static str>, primitive: &'static str) {
        let breaker = match primitive {
            "typing" => &mut self.breakers.typing,
            "status" => &mut self.breakers.status,
            "status_text" => &mut self.breakers.status_text,
            "reaction" => &mut self.breakers.reaction,
            _ => &mut self.breakers.progress,
        };
        match outcome {
            Ok(()) => {
                breaker.succeeded();
                tracing::debug!(
                    event = "gateway_progress_rendered",
                    transport = %self.transport,
                    primitive,
                    outcome = "ok"
                );
            }
            Err(category) => breaker.failed(&self.transport, primitive, category),
        }
    }
}

struct Live {
    surface: Surface,
    started: Instant,
    deadline: Deadline,
    message: Option<MessageRef>,
    typing: Schedule,
    keep_alive: KeepAliveTicks,
    edit: Throttle<Line>,
    stream: Throttle<()>,
}

struct Delivered(Surface);

impl Delivered {
    async fn cleanup(mut self) {
        self.0.cleanup(None).await;
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "one value per progress task, moved only at the Pending to Live transition"
)]
enum Phase {
    Pending(Surface),
    Live(Live),
}

impl Phase {
    async fn on_event(self, queued: QueuedEvent) -> Self {
        match self {
            Self::Pending(mut surface) => match queued.event {
                ProgressEvent::Started { max_steps, .. } => {
                    surface.state.of = max_steps;
                    Self::Live(Live::open(surface, Instant::now()).await)
                }
                ProgressEvent::ModelTurn { .. }
                | ProgressEvent::Answered { .. }
                | ProgressEvent::ToolStarted { .. }
                | ProgressEvent::ToolFinished { .. }
                | ProgressEvent::Attachment { .. }
                | ProgressEvent::Note { .. }
                | ProgressEvent::Steered { .. }
                | ProgressEvent::TextDelta { .. }
                | ProgressEvent::KeepAlive { .. }
                | ProgressEvent::Cancelled { .. }
                | ProgressEvent::Failed { .. }
                | ProgressEvent::Finished { .. } => Self::Pending(surface),
            },
            Self::Live(mut live) => {
                live.on_event(queued).await;
                Self::Live(live)
            }
        }
    }

    async fn when<'a, F, Fut>(&'a self, timer: F)
    where
        F: FnOnce(&'a Live) -> Fut,
        Fut: Future<Output = ()>,
    {
        match self {
            Self::Pending(_) => std::future::pending().await,
            Self::Live(live) => timer(live).await,
        }
    }

    async fn terminal(self, terminal: Terminal, text: StreamedText) -> (Delivered, bool) {
        match self {
            Self::Pending(mut surface) => {
                let delivered = surface.end(terminal, text, None, 0).await;
                (Delivered(surface), delivered)
            }
            Self::Live(live) => live.terminal(terminal, text).await,
        }
    }

    async fn abandon(self) {
        match self {
            Self::Pending(mut surface) => surface.cleanup(None).await,
            Self::Live(mut live) => live.surface.cleanup(live.message).await,
        }
    }
}

impl Live {
    async fn open(surface: Surface, now: Instant) -> Self {
        let typing_every = surface
            .driver
            .typing()
            .map_or(Duration::ZERO, |typing| typing.renew_every());
        let mut live = Self {
            started: now,
            deadline: Deadline::from_budget(now, surface.max_duration),
            message: None,
            typing: Schedule::idle(typing_every),
            keep_alive: KeepAliveTicks::start(&surface.keep_alive, now),
            edit: Throttle::new(),
            stream: Throttle::new(),
            surface,
        };
        live.open_indicators().await;
        live
    }

    fn writes_progress(&self) -> bool {
        let surface = &self.surface;
        surface.native()
            && surface.detail != ProgressDetail::Off
            && match surface.settings.progress {
                ProgressSurface::Off => false,
                ProgressSurface::Message => true,
                ProgressSurface::Auto => {
                    !surface.indicator_active || self.message.is_some() || surface.cancel_control()
                }
            }
            && !surface.streams()
    }

    async fn on_event(&mut self, queued: QueuedEvent) {
        let QueuedEvent {
            event,
            note_generation,
        } = queued;
        let state = &mut self.surface.state;
        match event {
            ProgressEvent::Started { max_steps, .. } => state.of = max_steps,
            ProgressEvent::ModelTurn { turn, of } => {
                state.note = None;
                state.turn = turn;
                state.of = of;
                self.render(Line::Status, false).await;
            }
            ProgressEvent::Answered { tool_calls, .. } => {
                state.word = None;
                self.render(Line::Status, tool_calls > 0).await;
            }
            ProgressEvent::ToolStarted {
                word,
                calls_used,
                calls_max,
                ..
            } => {
                state.set_word(word.as_str());
                state.calls = calls_used;
                state.calls_max = calls_max;
                self.render(Line::Status, true).await;
            }
            ProgressEvent::ToolFinished { .. } => {
                state.word = None;
                self.render(Line::Status, false).await;
            }
            ProgressEvent::Attachment { .. } => self.render(Line::Status, false).await,
            ProgressEvent::Note { text, eta } => {
                if note_generation
                    != self
                        .surface
                        .counters
                        .note_generation
                        .load(Ordering::Acquire)
                {
                    return;
                }
                state.note = Some(LiveNote {
                    text,
                    eta,
                    arrived: Instant::now(),
                    generation: note_generation,
                });
                self.render(Line::Status, true).await;
            }
            ProgressEvent::Steered { .. } => {
                state.note = None;
                self.render(Line::Status, false).await;
            }
            ProgressEvent::TextDelta { .. }
            | ProgressEvent::KeepAlive { .. }
            | ProgressEvent::Cancelled { .. }
            | ProgressEvent::Failed { .. }
            | ProgressEvent::Finished { .. } => {}
        }
    }

    async fn open_indicators(&mut self) {
        if !self.surface.native() {
            return;
        }
        let Some(target) = self.surface.target.clone() else {
            return;
        };
        let driver = Arc::clone(&self.surface.driver);
        let auto = self.surface.settings.progress == ProgressSurface::Auto;
        if !auto {
            self.surface.open_reaction().await;
            self.renew_typing().await;
        }
        if let Some(status) = driver.status() {
            self.surface.status_attempted = true;
            let outcome = bounded(status.set(&target, Status::Working)).await;
            self.surface.indicator_active = outcome.is_ok();
            self.surface.observe(outcome, "status");
            if auto && self.surface.indicator_active {
                return;
            }
        }
        if auto && driver.typing().is_some() {
            self.renew_typing().await;
            return;
        }
        if !self.surface.reaction_attempted {
            self.surface.open_reaction().await;
        }
    }

    async fn on_text(&mut self, text: StreamedText) {
        if text.text.as_str().is_empty() {
            self.surface.latest_text = None;
            self.stream.take();
            return;
        }
        self.surface.latest_text = Some(text);
        if self.surface.streams() {
            self.stream.defer(());
            self.flush_stream().await;
            return;
        }
        if !self.surface.posted_on_text {
            self.surface.posted_on_text = true;
            if self.message.is_none() && self.surface.status_text == StatusTextState::Waiting {
                self.render(Line::Status, true).await;
            }
        }
    }

    fn expire(&mut self) {
        self.deadline.spend();
        let surface = &self.surface;
        if surface.cancellation.cancel(CancelSource::Budget {
            limit: BudgetLimit::WallClock,
        }) {
            tracing::info!(
                event = "gateway_session_stop_requested",
                transport = %surface.transport,
                via = "wall-clock"
            );
        }
    }

    async fn keep_alive_tick(&mut self) {
        let now = Instant::now();
        let surface = &mut self.surface;
        let count = self.keep_alive.tick(&surface.keep_alive, now);
        record(&ProgressEvent::KeepAlive {
            elapsed: now.saturating_duration_since(self.started),
            count,
        });
        if surface.state.note.as_ref().is_some_and(|note| {
            now.saturating_duration_since(note.arrived)
                > note
                    .eta
                    .map_or(Duration::from_secs(120), |eta| eta.saturating_mul(2))
        }) {
            surface.state.note = None;
        }
        self.render(Line::KeepAlive, true).await;
        if self.keep_alive.exhausted() {
            self.surface.restore_native_status().await;
        }
    }

    async fn renew_typing(&mut self) {
        let surface = &mut self.surface;
        if !surface.coordination.running() || !surface.breakers.typing.allows() {
            self.typing.stop();
            return;
        }
        let Some(target) = surface.target.clone() else {
            self.typing.stop();
            return;
        };
        let driver = Arc::clone(&surface.driver);
        let Some(typing) = driver.typing() else {
            self.typing.stop();
            return;
        };
        let outcome = bounded(typing.renew(&target)).await;
        if surface.settings.progress == ProgressSurface::Auto && outcome.is_ok() {
            surface.indicator_active = true;
        }
        surface.observe(outcome, "typing");
        if surface.settings.progress == ProgressSurface::Auto && !surface.breakers.typing.allows() {
            surface.indicator_active = false;
            surface.open_reaction().await;
        }
        if self.surface.breakers.typing.allows() {
            self.typing.arm(Instant::now());
        } else {
            self.typing.stop();
        }
    }

    async fn render(&mut self, line: Line, allow_post: bool) {
        self.surface.clear_obsolete_note();
        if !self.surface.coordination.running() {
            return;
        }
        let writes_progress = self.writes_progress();
        let surface = &mut self.surface;
        let driver = Arc::clone(&surface.driver);
        let status_text = if surface.native()
            && surface.detail != ProgressDetail::Off
            && surface.settings.status_text
            && !writes_progress
            && (surface.state.note.is_some() || surface.status_text != StatusTextState::Waiting)
        {
            driver.status_text()
        } else {
            None
        };
        if surface.status_text != StatusTextState::Waiting && status_text.is_none() {
            surface.restore_native_status().await;
            return;
        }
        if !writes_progress && status_text.is_none() {
            return;
        }
        let Some(target) = surface.target.clone() else {
            return;
        };
        let progress = driver.progress();
        let min_interval = if let Some(status_text) = status_text {
            if !surface.breakers.status_text.allows() {
                surface.restore_native_status().await;
                return;
            }
            status_text.min_interval()
        } else {
            if !surface.breakers.progress.allows() {
                return;
            }
            let Some(progress) = progress else { return };
            if self.message.is_none() && !allow_post {
                return;
            }
            progress.limits().min_edit_interval
        };
        let now = Instant::now();
        if (self.message.is_some() || surface.status_text != StatusTextState::Waiting)
            && self.edit.waiting(now)
        {
            self.edit.defer(line);
            return;
        }
        self.edit.take();
        if surface.edits >= MAX_EDITS {
            if !surface.budget_reported {
                surface.budget_reported = true;
                tracing::debug!(
                    event = "gateway_progress_budget_exhausted",
                    transport = %surface.transport,
                    edits = surface.edits
                );
            }
            return;
        }
        surface.state.elapsed = now.saturating_duration_since(self.started);
        let text = surface.line(line);
        let creating = status_text.is_none() && self.message.is_none();
        let outcome = if let Some(status_text) = status_text {
            if surface.status_text == StatusTextState::Waiting {
                surface.status_text = StatusTextState::HandedOver;
                if let Some(status) = driver.status() {
                    surface.status_attempted = true;
                    let outcome = bounded(status.set(&target, Status::Idle)).await;
                    surface.observe(outcome, "status");
                }
            }
            bounded(status_text.show(&target, &text))
                .await
                .map(|()| None)
        } else {
            let Some(progress) = progress else { return };
            let cancel = surface.cancel_control();
            match &self.message {
                Some(message) => bounded(progress.edit(message, &text, cancel))
                    .await
                    .map(|()| None),
                None => bounded(progress.post(&target, &text, cancel))
                    .await
                    .map(Some),
            }
        };
        self.edit.hold(Instant::now(), min_interval);
        surface.edits = surface.edits.saturating_add(1);
        let (breaker, primitive) = if status_text.is_some() {
            (&mut surface.breakers.status_text, "status_text")
        } else {
            (&mut surface.breakers.progress, "progress")
        };
        match outcome {
            Ok(posted) => {
                breaker.succeeded();
                if let Some(message) = posted {
                    self.message = Some(message);
                }
                tracing::debug!(
                    event = "gateway_progress_rendered",
                    transport = %surface.transport,
                    primitive,
                    outcome = "ok"
                );
            }
            Err(category) if creating && category == DEADLINE_MISSED => {
                breaker.orphaned(&surface.transport, primitive);
            }
            Err(category) => breaker.failed(&surface.transport, primitive, category),
        }
        if status_text.is_some()
            && (!surface.breakers.status_text.allows()
                || driver.status_text().is_none()
                || self.keep_alive.exhausted()
                || surface.edits >= MAX_EDITS)
        {
            surface.restore_native_status().await;
        }
    }

    async fn flush_stream(&mut self) {
        let surface = &mut self.surface;
        if !surface.streams()
            || !surface.coordination.running()
            || !surface.breakers.stream.allows()
        {
            self.stream.take();
            return;
        }
        if self.stream.waiting(Instant::now()) {
            return;
        }
        let (Some(target), Some(latest)) = (surface.target.clone(), surface.latest_text.clone())
        else {
            self.stream.take();
            return;
        };
        let driver = Arc::clone(&surface.driver);
        let Some(stream) = driver.stream() else {
            self.stream.take();
            return;
        };
        let limits = stream.limits();
        let truncated = latest.text.as_str().chars().count() > limits.max_chars;
        let text = StreamedText {
            generation: latest.generation,
            text: if truncated {
                latest.text.truncated(limits.max_chars)
            } else {
                latest.text
            },
            truncated,
        };
        let chars = text.text.as_str().chars().count();
        let cancel = surface.cancel_control();
        let creating = self.message.is_none();
        let outcome = bounded(stream.show(&target, self.message.as_ref(), &text, cancel)).await;
        self.stream.take();
        self.stream.hold(Instant::now(), limits.min_interval);
        match outcome {
            Ok(message) => {
                surface.breakers.stream.succeeded();
                self.message = Some(message);
                surface.streaming = true;
                tracing::debug!(
                    event = "gateway_progress_rendered",
                    transport = %surface.transport,
                    primitive = "stream",
                    outcome = "ok",
                    chars
                );
            }
            Err(category) if creating && category == DEADLINE_MISSED => {
                surface
                    .breakers
                    .stream
                    .orphaned(&surface.transport, "stream");
            }
            Err(category) => surface
                .breakers
                .stream
                .failed(&surface.transport, "stream", category),
        }
    }

    async fn terminal(self, terminal: Terminal, text: StreamedText) -> (Delivered, bool) {
        let Self {
            mut surface,
            message,
            keep_alive,
            ..
        } = self;
        let delivered = surface.end(terminal, text, message, keep_alive.fired).await;
        (Delivered(surface), delivered)
    }
}

fn ended(partial: &str, ending: &str) -> String {
    if partial.is_empty() {
        return ending.to_owned();
    }
    format!("{partial}\n\n{ending}")
}

/// Single-tasked with one writer; nothing but this call's own deadline ever cancels an in-flight
/// call, since a dropped HTTP future cannot retract bytes already sent.
async fn run(
    surface: Surface,
    mut events: mpsc::Receiver<QueuedEvent>,
    mut text: watch::Receiver<StreamedText>,
    mut terminal: oneshot::Receiver<TerminalRequest>,
    cancellation: SessionCancellation,
) {
    let mut events_open = true;
    let mut text_open = true;
    let coordination = Arc::clone(&surface.coordination);
    let mut phase = Phase::Pending(surface);
    loop {
        tokio::select! {
            biased;
            request = &mut terminal => {
                let Ok(request) = request else { break };
                let latest = text.borrow_and_update().clone();
                let (ending, delivered) = phase.terminal(request.terminal, latest).await;
                // Cleanup's Idle write must land before the caller can admit this thread's next
                // turn, since Slack never reverts native status on its own when the reply posts.
                ending.cleanup().await;
                if request.done.send(delivered).is_err() {
                    tracing::debug!(event = "gateway_progress_terminal_unobserved");
                }
                return;
            }
            () = cancellation.cancelled() => {
                let by = cancellation.source().unwrap_or(CancelSource::Operator);
                let latest = text.borrow_and_update().clone();
                let (ending, _) = phase
                    .terminal(Terminal::Stopped(StopCause::Cancelled(by)), latest)
                    .await;
                ending.cleanup().await;
                coordination.finish();
                return;
            }
            () = coordination.finished() => break,
            () = phase.when(|live| live.deadline.fired()) => {
                if let Phase::Live(live) = &mut phase {
                    live.expire();
                }
            }
            event = events.recv(), if events_open => match event {
                Some(event) => phase = phase.on_event(event).await,
                None => events_open = false,
            },
            changed = text.changed(), if text_open => match changed {
                Ok(()) => {
                    let latest = text.borrow_and_update().clone();
                    if let Phase::Live(live) = &mut phase {
                        live.on_text(latest).await;
                    }
                }
                Err(_) => text_open = false,
            },
            () = phase.when(|live| live.typing.due()) => {
                if let Phase::Live(live) = &mut phase {
                    live.renew_typing().await;
                }
            }
            () = phase.when(|live| live.keep_alive.due()) => {
                if let Phase::Live(live) = &mut phase {
                    live.keep_alive_tick().await;
                }
            }
            () = phase.when(|live| live.edit.due()) => {
                if let Phase::Live(live) = &mut phase
                    && let Some(line) = live.edit.take()
                {
                    live.render(line, false).await;
                }
            }
            () = phase.when(|live| live.stream.due()) => {
                if let Phase::Live(live) = &mut phase {
                    live.flush_stream().await;
                }
            }
        }
        if let Phase::Live(live) = &mut phase
            && live.surface.clear_obsolete_note()
        {
            live.render(Line::Status, false).await;
        }
    }
    phase.abandon().await;
    coordination.finish();
}
