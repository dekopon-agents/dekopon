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
        cancel_label,
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
    Failed(String),
    Stopped(StopCause),
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
        let surface = Pending {
            inputs,
            coordination: Arc::clone(&coordination),
            counters,
        };
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

struct Deadline(Option<Instant>);

impl Deadline {
    fn new(now: Instant, budget: Option<Duration>) -> Self {
        Self(budget.map(|budget| now + budget))
    }

    async fn fired(&mut self) {
        match self.0 {
            Some(at) => {
                tokio::time::sleep_until(at).await;
                self.0 = None;
            }
            None => std::future::pending().await,
        }
    }
}

struct Schedule<T> {
    next: Instant,
    every: Duration,
    pending: Option<T>,
}

impl<T> Schedule<T> {
    fn new(now: Instant) -> Self {
        Self {
            next: now,
            every: Duration::ZERO,
            pending: None,
        }
    }

    fn arm(&mut self, from: Instant, every: Duration, value: T) {
        self.every = every;
        self.next = from + self.every;
        self.pending = Some(value);
    }

    fn cooldown(&mut self, every: Duration) {
        self.every = every;
        self.next = Instant::now() + self.every;
        self.clear();
    }

    fn ready(&mut self, value: T) -> Option<T> {
        if Instant::now() < self.next {
            self.pending = Some(value);
            None
        } else {
            self.clear();
            Some(value)
        }
    }

    fn clear(&mut self) {
        self.pending = None;
    }

    fn active(&self) -> bool {
        self.pending.is_some()
    }

    async fn due(&self) {
        match &self.pending {
            Some(_) if Instant::now() >= self.next => {}
            Some(_) => tokio::time::sleep_until(self.next).await,
            None => std::future::pending().await,
        }
    }
}

struct Pending {
    inputs: ProgressInputs,
    coordination: Arc<Coordination>,
    counters: Arc<ProgressCounters>,
}

struct Delivered {
    accepted: bool,
}

impl Pending {
    async fn open(self, now: Instant) -> Live {
        let mut live = Live::new(self.inputs, self.coordination, self.counters, now);
        live.schedule_keep_alive(now);
        live.open_indicators().await;
        live
    }

    async fn terminal(self, terminal: Terminal) -> Delivered {
        record_terminal(&terminal, &self.counters, 0, 0);
        let reply = match terminal {
            Terminal::Answered(reply) => Some(reply),
            Terminal::Stopped(cause) => Some(OutboundReply::text(
                crate::transport::bound_outbound(cause.line(&self.inputs.liveness.templates)),
            )),
            Terminal::Failed(line) => Some(OutboundReply::text(line)),
            Terminal::Silent => None,
        };
        let accepted = match reply {
            Some(reply) => match self.inputs.driver.reply(&self.inputs.reply, reply).await {
                Ok(()) => true,
                Err(error) => {
                    tracing::error!(event = "gateway_reply_failed", category = error.category());
                    false
                }
            },
            None => false,
        };
        self.coordination.finish();
        Delivered { accepted }
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "the existing renderer independently tracks each native surface's attempted writes"
)]
struct Live {
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
    state: RenderState,
    started: Instant,
    deadline: Deadline,
    message: Option<MessageRef>,
    streaming: bool,
    latest_text: Option<StreamedText>,
    posted_on_text: bool,
    typing: Schedule<()>,
    keep_alive_schedule: Schedule<()>,
    keep_alives: u32,
    edits: u32,
    budget_reported: bool,
    edit: Schedule<Line>,
    stream: Schedule<()>,
    breakers: Breakers,
    indicator_active: bool,
    status_attempted: bool,
    status_text: StatusTextState,
    reaction_attempted: bool,
}

impl Live {
    fn new(
        inputs: ProgressInputs,
        coordination: Arc<Coordination>,
        counters: Arc<ProgressCounters>,
        now: Instant,
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
            state: RenderState::default(),
            started: now,
            deadline: Deadline::new(now, inputs.max_duration),
            message: None,
            streaming: false,
            latest_text: None,
            posted_on_text: false,
            typing: Schedule::new(now),
            keep_alive_schedule: Schedule::new(now),
            keep_alives: 0,
            edits: 0,
            budget_reported: false,
            edit: Schedule::new(now),
            stream: Schedule::new(now),
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

    fn writes_progress(&self) -> bool {
        self.native()
            && self.detail != ProgressDetail::Off
            && match self.settings.progress {
                ProgressSurface::Off => false,
                ProgressSurface::Message => true,
                ProgressSurface::Auto => {
                    !self.indicator_active || self.message.is_some() || self.cancel_control()
                }
            }
            && !self.streams()
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

    async fn on_event(&mut self, queued: QueuedEvent) {
        let QueuedEvent {
            event,
            note_generation,
        } = queued;
        match event {
            ProgressEvent::Started { max_steps, .. } => {
                self.state.of = max_steps;
            }
            ProgressEvent::ModelTurn { turn, of } => {
                self.state.note = None;
                self.state.turn = turn;
                self.state.of = of;
                self.render(Line::Status, false).await;
            }
            ProgressEvent::Answered { tool_calls, .. } => {
                self.state.word = None;
                self.render(Line::Status, tool_calls > 0).await;
            }
            ProgressEvent::ToolStarted {
                word,
                calls_used,
                calls_max,
                ..
            } => {
                self.state.set_word(word.as_str());
                self.state.calls = calls_used;
                self.state.calls_max = calls_max;
                self.render(Line::Status, true).await;
            }
            ProgressEvent::ToolFinished { .. } => {
                self.state.word = None;
                self.render(Line::Status, false).await;
            }
            ProgressEvent::Attachment { .. } => self.render(Line::Status, false).await,
            ProgressEvent::Note { text, eta } => {
                if note_generation != self.counters.note_generation.load(Ordering::Acquire) {
                    return;
                }
                self.state.note = Some(LiveNote {
                    text,
                    eta,
                    arrived: Instant::now(),
                    generation: note_generation,
                });
                self.render(Line::Status, true).await;
            }
            ProgressEvent::Steered { .. } => {
                self.state.note = None;
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
        if !self.native() {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let driver = Arc::clone(&self.driver);
        let auto = self.settings.progress == ProgressSurface::Auto;
        if !auto {
            self.open_reaction().await;
            self.renew_typing().await;
        }
        if let Some(status) = driver.status() {
            self.status_attempted = true;
            let outcome = bounded(status.set(&target, Status::Working)).await;
            self.indicator_active = outcome.is_ok();
            self.observe(outcome, "status");
            if auto && self.indicator_active {
                return;
            }
        }
        if auto && driver.typing().is_some() {
            self.renew_typing().await;
            return;
        }
        if !self.reaction_attempted {
            self.open_reaction().await;
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

    async fn on_text(&mut self, text: StreamedText) {
        if text.text.as_str().is_empty() {
            self.latest_text = None;
            self.stream.clear();
            return;
        }
        self.latest_text = Some(text);
        if self.streams() {
            self.flush_stream().await;
            return;
        }
        if !self.posted_on_text {
            self.posted_on_text = true;
            if self.message.is_none() && self.status_text == StatusTextState::Waiting {
                self.render(Line::Status, true).await;
            }
        }
    }

    async fn keep_alive_due(&mut self) {
        let now = Instant::now();
        self.keep_alives = self.keep_alives.saturating_add(1);
        self.schedule_keep_alive(now);
        let elapsed = now.saturating_duration_since(self.started);
        record(&ProgressEvent::KeepAlive {
            elapsed,
            count: self.keep_alives,
        });
        if self.state.note.as_ref().is_some_and(|note| {
            now.saturating_duration_since(note.arrived)
                > note
                    .eta
                    .map_or(Duration::from_secs(120), |eta| eta.saturating_mul(2))
        }) {
            self.state.note = None;
        }
        self.render(Line::KeepAlive, true).await;
        if !self.keep_alive_schedule.active() {
            self.restore_native_status().await;
        }
    }

    async fn renew_typing(&mut self) {
        if !self.coordination.running() || !self.breakers.typing.allows() {
            self.typing.clear();
            return;
        }
        let Some(target) = self.target.clone() else {
            self.typing.clear();
            return;
        };
        let driver = Arc::clone(&self.driver);
        let Some(typing) = driver.typing() else {
            self.typing.clear();
            return;
        };
        let outcome = bounded(typing.renew(&target)).await;
        if self.settings.progress == ProgressSurface::Auto && outcome.is_ok() {
            self.indicator_active = true;
        }
        self.observe(outcome, "typing");
        if self.settings.progress == ProgressSurface::Auto && !self.breakers.typing.allows() {
            self.indicator_active = false;
            self.open_reaction().await;
        }
        if self.breakers.typing.allows() {
            self.typing.arm(Instant::now(), typing.renew_every(), ());
        } else {
            self.typing.clear();
        }
    }

    fn schedule_keep_alive(&mut self, from: Instant) {
        let keep_alive = &self.keep_alive;
        if self.keep_alives >= keep_alive.max {
            self.keep_alive_schedule.clear();
            return;
        }
        let fired = self.keep_alives as usize;
        let gap = match keep_alive.at.get(fired) {
            Some(offset) => {
                let previous = fired
                    .checked_sub(1)
                    .and_then(|index| keep_alive.at.get(index))
                    .copied()
                    .unwrap_or_default();
                offset.saturating_sub(previous)
            }
            None => keep_alive.every,
        };
        self.keep_alive_schedule.arm(from, gap, ());
    }

    async fn render(&mut self, line: Line, allow_post: bool) {
        self.clear_obsolete_note();
        if !self.coordination.running() {
            return;
        }
        let driver = Arc::clone(&self.driver);
        let status_text = if self.native()
            && self.detail != ProgressDetail::Off
            && self.settings.status_text
            && !self.writes_progress()
            && (self.state.note.is_some() || self.status_text != StatusTextState::Waiting)
        {
            driver.status_text()
        } else {
            None
        };
        if self.status_text != StatusTextState::Waiting && status_text.is_none() {
            self.restore_native_status().await;
            return;
        }
        if !self.writes_progress() && status_text.is_none() {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let progress = driver.progress();
        let min_interval = if let Some(status_text) = status_text {
            if !self.breakers.status_text.allows() {
                self.restore_native_status().await;
                return;
            }
            status_text.min_interval()
        } else {
            if !self.breakers.progress.allows() {
                return;
            }
            let Some(progress) = progress else { return };
            if self.message.is_none() && !allow_post {
                return;
            }
            progress.limits().min_edit_interval
        };
        let now = Instant::now();
        if (self.message.is_some() || self.status_text != StatusTextState::Waiting)
            && self.edit.ready(line).is_none()
        {
            return;
        }
        if self.edits >= MAX_EDITS {
            if !self.budget_reported {
                self.budget_reported = true;
                tracing::debug!(
                    event = "gateway_progress_budget_exhausted",
                    transport = %self.transport,
                    edits = self.edits
                );
            }
            self.edit.clear();
            return;
        }
        self.edit.clear();
        self.state.elapsed = now.saturating_duration_since(self.started);
        let text = self.line(line);
        let creating = status_text.is_none() && self.message.is_none();
        let outcome = if let Some(status_text) = status_text {
            if self.status_text == StatusTextState::Waiting {
                self.status_text = StatusTextState::HandedOver;
                if let Some(status) = driver.status() {
                    self.status_attempted = true;
                    let outcome = bounded(status.set(&target, Status::Idle)).await;
                    self.observe(outcome, "status");
                }
            }
            bounded(status_text.show(&target, &text))
                .await
                .map(|()| None)
        } else {
            let Some(progress) = progress else { return };
            let cancel = self.cancel_control();
            match self.message.clone() {
                Some(message) => bounded(progress.edit(&message, &text, cancel))
                    .await
                    .map(|()| None),
                None => bounded(progress.post(&target, &text, cancel))
                    .await
                    .map(Some),
            }
        };
        self.edit.cooldown(min_interval);
        self.edits = self.edits.saturating_add(1);
        let (breaker, primitive) = if status_text.is_some() {
            (&mut self.breakers.status_text, "status_text")
        } else {
            (&mut self.breakers.progress, "progress")
        };
        match outcome {
            Ok(posted) => {
                breaker.succeeded();
                if let Some(message) = posted {
                    self.message = Some(message);
                }
                tracing::debug!(
                    event = "gateway_progress_rendered",
                    transport = %self.transport,
                    primitive,
                    outcome = "ok"
                );
            }
            Err(category) if creating && category == DEADLINE_MISSED => {
                breaker.orphaned(&self.transport, primitive)
            }
            Err(category) => breaker.failed(&self.transport, primitive, category),
        }
        if status_text.is_some()
            && (!self.breakers.status_text.allows()
                || driver.status_text().is_none()
                || !self.keep_alive_schedule.active()
                || self.edits >= MAX_EDITS)
        {
            self.restore_native_status().await;
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

    async fn flush_stream(&mut self) {
        if !self.streams() || !self.coordination.running() || !self.breakers.stream.allows() {
            self.stream.clear();
            return;
        }
        if self.stream.ready(()).is_none() {
            return;
        }
        let (Some(target), Some(latest)) = (self.target.clone(), self.latest_text.clone()) else {
            self.stream.clear();
            return;
        };
        let driver = Arc::clone(&self.driver);
        let Some(stream) = driver.stream() else {
            self.stream.clear();
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
        let cancel = self.cancel_control();
        let creating = self.message.is_none();
        let outcome = bounded(stream.show(&target, self.message.as_ref(), &text, cancel)).await;
        self.stream.clear();
        self.stream.cooldown(limits.min_interval);
        match outcome {
            Ok(message) => {
                self.breakers.stream.succeeded();
                self.message = Some(message);
                self.streaming = true;
                tracing::debug!(
                    event = "gateway_progress_rendered",
                    transport = %self.transport,
                    primitive = "stream",
                    outcome = "ok",
                    chars
                );
            }
            Err(category) if creating && category == DEADLINE_MISSED => {
                self.breakers.stream.orphaned(&self.transport, "stream");
            }
            Err(category) => self
                .breakers
                .stream
                .failed(&self.transport, "stream", category),
        }
    }

    async fn terminal(mut self, terminal: Terminal, text: StreamedText) -> Delivered {
        self.state.note = None;
        let generation = text.generation;
        self.latest_text = (!text.text.as_str().is_empty()).then_some(text);
        self.stream.clear();
        self.coordination.seal();
        self.clear_status_text().await;
        record_terminal(&terminal, &self.counters, self.edits, self.keep_alives);
        let streamed = self.streamed_text();
        let accepted = match terminal {
            Terminal::Answered(reply) => self.finalize(reply, generation).await,
            Terminal::Failed(line) => match streamed {
                Some(partial) => {
                    self.finalize(OutboundReply::text(ended(&partial, &line)), generation)
                        .await
                }
                None => {
                    self.discard().await;
                    self.deliver(OutboundReply::text(line)).await
                }
            },
            Terminal::Stopped(cause) => {
                let stopped =
                    crate::transport::bound_outbound(cause.line(&self.liveness.templates));
                let reply = match streamed {
                    Some(partial) => ended(&partial, &stopped),
                    None => stopped,
                };
                self.finalize(OutboundReply::text(reply), generation).await
            }
            Terminal::Silent => {
                match streamed.filter(|partial| !partial.is_empty()) {
                    Some(partial) => {
                        self.finalize(OutboundReply::text(partial), generation)
                            .await;
                    }
                    None => self.discard().await,
                }
                false
            }
        };
        self.cleanup().await;
        self.coordination.finish();
        Delivered { accepted }
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

    async fn finalize(&mut self, reply: OutboundReply, generation: u64) -> bool {
        if let Some(message) = self.message.clone() {
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
                    self.message = None;
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
                        self.message = None;
                        return self.deliver(reply).await;
                    }
                }
                None => {}
            }
            self.discard().await;
        }
        self.deliver(reply).await
    }

    async fn discard(&mut self) {
        let Some(message) = self.message.take() else {
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

    async fn cleanup(&mut self) {
        self.state.note = None;
        self.clear_status_text().await;
        if !self.streaming {
            self.discard().await;
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

fn record_terminal(terminal: &Terminal, counters: &ProgressCounters, edits: u32, keep_alives: u32) {
    tracing::info!(
        target: "dekopon_gatewayd::audit",
        {
            audit.event = "gateway.progress",
            kind = match terminal {
                Terminal::Answered(_) => "terminal_answered",
                Terminal::Stopped(StopCause::Cancelled(_)) => "terminal_cancelled",
                Terminal::Stopped(StopCause::Model(_) | StopCause::EmptyAnswer | StopCause::MaxSteps | StopCause::SessionTask)
                | Terminal::Failed(_) => "terminal_failed",
                Terminal::Silent => "terminal_silent",
            },
            by = match terminal {
                Terminal::Stopped(StopCause::Cancelled(by)) => Some(cancel_label(*by)),
                Terminal::Stopped(StopCause::Model(_) | StopCause::EmptyAnswer | StopCause::MaxSteps | StopCause::SessionTask)
                | Terminal::Answered(_) | Terminal::Failed(_) | Terminal::Silent => None,
            },
            edits = edits,
            keep_alives = keep_alives,
            stream.deltas = counters.deltas.load(Ordering::Relaxed),
            progress.dropped = counters.dropped.load(Ordering::Relaxed),
            progress.notes_dropped = counters.notes_dropped.load(Ordering::Relaxed),
        },
        "gateway progress"
    );
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
    pending: Pending,
    mut events: mpsc::Receiver<QueuedEvent>,
    mut text: watch::Receiver<StreamedText>,
    mut terminal: oneshot::Receiver<TerminalRequest>,
    cancellation: SessionCancellation,
) {
    let coordination = Arc::clone(&pending.coordination);
    let mut surface = loop {
        tokio::select! {
            biased;
            request = &mut terminal => {
                if let Ok(request) = request {
                    let delivered = pending.terminal(request.terminal).await;
                    acknowledge(request.done, delivered);
                }
                return;
            }
            () = cancellation.cancelled() => {
                let by = cancellation.source().unwrap_or(CancelSource::Operator);
                pending.terminal(Terminal::Stopped(StopCause::Cancelled(by))).await;
                return;
            }
            () = coordination.finished() => return,
            event = events.recv() => match event {
                Some(event) => match event.event {
                    ProgressEvent::Started { .. } => {
                        let mut live = pending.open(Instant::now()).await;
                        live.on_event(event).await;
                        break live;
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
                    | ProgressEvent::Finished { .. } => {}
                },
                None => return,
            },
        }
    };
    let mut events_open = true;
    let mut text_open = true;
    loop {
        tokio::select! {
            biased;
            request = &mut terminal => {
                let Ok(request) = request else { break };
                let latest = text.borrow_and_update().clone();
                let delivered = surface.terminal(request.terminal, latest).await;
                acknowledge(request.done, delivered);
                return;
            }
            () = cancellation.cancelled() => {
                let by = cancellation.source().unwrap_or(CancelSource::Operator);
                let latest = text.borrow_and_update().clone();
                surface.terminal(Terminal::Stopped(StopCause::Cancelled(by)), latest).await;
                return;
            }
            () = coordination.finished() => break,
            () = surface.deadline.fired() => {
                if cancellation.cancel(CancelSource::Budget { limit: BudgetLimit::WallClock }) {
                    tracing::info!(event = "gateway_session_stop_requested", transport = %surface.transport, via = "wall-clock");
                }
            }
            event = events.recv(), if events_open => match event {
                Some(event) => surface.on_event(event).await,
                None => events_open = false,
            },
            changed = text.changed(), if text_open => match changed {
                Ok(()) => {
                    let latest = text.borrow_and_update().clone();
                    surface.on_text(latest).await;
                }
                Err(_) => text_open = false,
            },
            () = surface.typing.due() => surface.renew_typing().await,
            () = surface.keep_alive_schedule.due() => surface.keep_alive_due().await,
            () = surface.edit.due() => {
                if let Some(line) = surface.edit.pending.take() {
                    surface.render(line, false).await;
                }
            }
            () = surface.stream.due() => surface.flush_stream().await,
        }
        if surface.clear_obsolete_note() {
            surface.render(Line::Status, false).await;
        }
    }
    surface.cleanup().await;
    coordination.finish();
}

fn acknowledge(done: oneshot::Sender<bool>, delivered: Delivered) {
    if done.send(delivered.accepted).is_err() {
        tracing::debug!(event = "gateway_progress_terminal_unobserved");
    }
}
