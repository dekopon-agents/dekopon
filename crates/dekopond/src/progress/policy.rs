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
use dekopon_model::ModelText;
use tokio::{
    sync::{Notify, mpsc, oneshot, watch},
    time::Instant,
};

use crate::{
    config::{LivenessMode, LivenessSettings, ProgressSurface, ResolvedLiveness},
    progress::{
        KeepAlive,
        adapter::{EVENT_QUEUE, ProgressAdapter, ProgressCounters, record},
        cancel_label,
        text::{ProgressDetail, ProgressText, RenderState},
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

async fn bounded<T>(
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
    Cancelled { by: CancelSource },
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
        let (text_tx, text_rx) = watch::channel(ModelText::default());
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
            reason = "detached on purpose: post-answer cleanup outlives the session so its \
                      admission permit frees now; bounded by EVENT_QUEUE and each driver call's \
                      timeout, and it ends once the terminal request and event queue are done"
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
    reaction: Breaker,
    progress: Breaker,
    stream: Breaker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Line {
    Status,
    KeepAlive,
}

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
    started: Option<Instant>,
    deadline: Option<Instant>,
    message: Option<MessageRef>,
    streaming: bool,
    latest_text: Option<ModelText>,
    posted_on_text: bool,
    next_typing: Option<Instant>,
    next_keep_alive: Option<Instant>,
    keep_alives: u32,
    edits: u32,
    budget_reported: bool,
    pending: Option<Line>,
    earliest_edit: Option<Instant>,
    next_stream: Option<Instant>,
    stream_pending: bool,
    breakers: Breakers,
    indicator_active: bool,
    status_attempted: bool,
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
            started: None,
            deadline: None,
            message: None,
            streaming: false,
            latest_text: None,
            posted_on_text: false,
            next_typing: None,
            next_keep_alive: None,
            keep_alives: 0,
            edits: 0,
            budget_reported: false,
            pending: None,
            earliest_edit: None,
            next_stream: None,
            stream_pending: false,
            breakers: Breakers::default(),
            indicator_active: false,
            status_attempted: false,
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

    fn next_wake(&self) -> Option<Instant> {
        [
            self.deadline,
            self.next_typing,
            self.next_keep_alive,
            self.pending.and(self.earliest_edit),
            self.stream_pending.then_some(self.next_stream).flatten(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    async fn on_event(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::Started { max_steps, .. } => {
                self.state.of = max_steps;
                let now = Instant::now();
                self.started = Some(now);
                self.deadline = self.max_duration.map(|budget| now + budget);
                self.schedule_keep_alive(now);
                self.open().await;
            }
            ProgressEvent::ModelTurn { turn, of } => {
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
            ProgressEvent::TextDelta { .. }
            | ProgressEvent::KeepAlive { .. }
            | ProgressEvent::Cancelled { .. }
            | ProgressEvent::Failed { .. }
            | ProgressEvent::Finished { .. } => {}
        }
    }

    async fn open(&mut self) {
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

    async fn on_text(&mut self, text: ModelText) {
        let empty = text.as_str().is_empty();
        self.latest_text = Some(text);
        if self.streams() {
            self.stream_pending = true;
            self.flush_stream().await;
            return;
        }
        if !self.posted_on_text && !empty {
            self.posted_on_text = true;
            if self.message.is_none() {
                self.render(Line::Status, true).await;
            }
        }
    }

    async fn on_timer(&mut self) {
        let now = Instant::now();
        if let Some(deadline) = self.deadline
            && now >= deadline
        {
            self.deadline = None;
            if self.cancellation.cancel(CancelSource::Budget {
                limit: BudgetLimit::WallClock,
            }) {
                tracing::info!(
                    event = "gateway_session_stop_requested",
                    transport = %self.transport,
                    via = "wall-clock"
                );
            }
            return;
        }
        if let Some(next) = self.next_typing
            && now >= next
        {
            self.renew_typing().await;
        }
        if let Some(next) = self.next_keep_alive
            && now >= next
        {
            self.keep_alives = self.keep_alives.saturating_add(1);
            self.schedule_keep_alive(now);
            let elapsed = now.saturating_duration_since(self.started.unwrap_or(now));
            record(&ProgressEvent::KeepAlive {
                elapsed,
                count: self.keep_alives,
            });
            self.render(Line::KeepAlive, true).await;
        }
        // Cleared before attempting the render, not after, so a declined render does not leave a
        // past deadline that wakes this loop again immediately.
        if let Some(line) = self.pending
            && self.earliest_edit.is_none_or(|earliest| now >= earliest)
        {
            self.pending = None;
            self.render(line, false).await;
        }
        if self.stream_pending {
            self.flush_stream().await;
        }
    }

    async fn renew_typing(&mut self) {
        if !self.coordination.running() || !self.breakers.typing.allows() {
            self.next_typing = None;
            return;
        }
        let Some(target) = self.target.clone() else {
            self.next_typing = None;
            return;
        };
        let driver = Arc::clone(&self.driver);
        let Some(typing) = driver.typing() else {
            self.next_typing = None;
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
        self.next_typing = self
            .breakers
            .typing
            .allows()
            .then(|| Instant::now() + typing.renew_every());
    }

    fn schedule_keep_alive(&mut self, from: Instant) {
        let keep_alive = &self.keep_alive;
        if self.keep_alives >= keep_alive.max {
            self.next_keep_alive = None;
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
        self.next_keep_alive = Some(from + gap);
    }

    async fn render(&mut self, line: Line, allow_post: bool) {
        if !self.writes_progress() || !self.coordination.running() {
            return;
        }
        if !self.breakers.progress.allows() {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let driver = Arc::clone(&self.driver);
        let Some(progress) = driver.progress() else {
            return;
        };
        if self.message.is_none() && !allow_post {
            return;
        }
        let now = Instant::now();
        if self.message.is_some()
            && let Some(earliest) = self.earliest_edit
            && now < earliest
        {
            self.pending = Some(line);
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
            self.pending = None;
            return;
        }
        self.pending = None;
        self.state.elapsed = now.saturating_duration_since(self.started.unwrap_or(now));
        let text = self.line(line);
        let cancel = self.cancel_control();
        let limits = progress.limits();
        let creating = self.message.is_none();
        let outcome = match self.message.clone() {
            Some(message) => bounded(progress.edit(&message, &text, cancel))
                .await
                .map(|()| None),
            None => bounded(progress.post(&target, &text, cancel))
                .await
                .map(Some),
        };
        self.earliest_edit = Some(Instant::now() + limits.min_edit_interval);
        self.edits = self.edits.saturating_add(1);
        match outcome {
            Ok(posted) => {
                self.breakers.progress.succeeded();
                if let Some(message) = posted {
                    self.message = Some(message);
                }
                tracing::debug!(
                    event = "gateway_progress_rendered",
                    transport = %self.transport,
                    primitive = "progress",
                    outcome = "ok"
                );
            }
            Err(category) if creating && category == DEADLINE_MISSED => {
                self.breakers.progress.orphaned(&self.transport, "progress")
            }
            Err(category) => self
                .breakers
                .progress
                .failed(&self.transport, "progress", category),
        }
    }

    fn line(&self, line: Line) -> ProgressText {
        let templates = &self.liveness.templates;
        match line {
            Line::KeepAlive => templates.keep_alive(self.detail, &self.state),
            Line::Status if self.state.word.is_some() => templates.tool(self.detail, &self.state),
            Line::Status => templates.working(self.detail, &self.state),
        }
    }

    async fn flush_stream(&mut self) {
        if !self.streams() || !self.coordination.running() || !self.breakers.stream.allows() {
            self.stream_pending = false;
            return;
        }
        let now = Instant::now();
        if let Some(next) = self.next_stream
            && now < next
        {
            return;
        }
        let (Some(target), Some(latest)) = (self.target.clone(), self.latest_text.clone()) else {
            self.stream_pending = false;
            return;
        };
        let driver = Arc::clone(&self.driver);
        let Some(stream) = driver.stream() else {
            self.stream_pending = false;
            return;
        };
        let limits = stream.limits();
        let truncated = latest.as_str().chars().count() > limits.max_chars;
        let text = StreamedText {
            text: if truncated {
                latest.truncated(limits.max_chars)
            } else {
                latest
            },
            truncated,
        };
        let chars = text.text.as_str().chars().count();
        let cancel = self.cancel_control();
        let creating = self.message.is_none();
        let outcome = bounded(stream.show(&target, self.message.as_ref(), &text, cancel)).await;
        self.stream_pending = false;
        self.next_stream = Some(Instant::now() + limits.min_interval);
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

    async fn terminal(&mut self, terminal: Terminal) -> bool {
        self.coordination.seal();
        self.record_terminal(&terminal);
        let streamed = self.streamed_text();
        match terminal {
            Terminal::Answered(reply) => self.finalize(reply).await,
            Terminal::Failed(line) => match streamed {
                Some(partial) => {
                    self.finalize(OutboundReply::text(ended(&partial, &line)))
                        .await
                }
                None => {
                    self.discard().await;
                    self.deliver(OutboundReply::text(line)).await
                }
            },
            Terminal::Cancelled { .. } => {
                let stopped = self.liveness.templates.stopped().to_owned();
                let reply = match streamed {
                    Some(partial) => ended(&partial, &stopped),
                    None => stopped,
                };
                self.finalize(OutboundReply::text(reply)).await
            }
            Terminal::Silent => {
                match streamed {
                    Some(partial) => {
                        self.finalize(OutboundReply::text(partial)).await;
                    }
                    None => self.discard().await,
                }
                false
            }
        }
    }

    fn streamed_text(&self) -> Option<String> {
        self.streaming.then(|| {
            self.latest_text
                .as_ref()
                .map(ModelText::as_str)
                .unwrap_or_default()
                .to_owned()
        })
    }

    async fn finalize(&mut self, reply: OutboundReply) -> bool {
        if let Some(message) = self.message.clone() {
            let driver = Arc::clone(&self.driver);
            let finalized = if self.streaming {
                match driver.stream() {
                    Some(stream) => Some(bounded(stream.finalize(&message, &reply)).await),
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
        if self.streaming {
            return;
        }
        let driver = Arc::clone(&self.driver);
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

    fn record_terminal(&self, terminal: &Terminal) {
        tracing::info!(
            target: "dekopond::audit",
            {
                audit.event = "gateway.progress",
                kind = match terminal {
                    Terminal::Answered(_) => "terminal_answered",
                    Terminal::Failed(_) => "terminal_failed",
                    Terminal::Cancelled { .. } => "terminal_cancelled",
                    Terminal::Silent => "terminal_silent",
                },
                by = match terminal {
                    Terminal::Cancelled { by } => Some(cancel_label(*by)),
                    Terminal::Answered(_) | Terminal::Failed(_) | Terminal::Silent => None,
                },
                edits = self.edits,
                keep_alives = self.keep_alives,
                stream.deltas = self.counters.deltas.load(Ordering::Relaxed),
                progress.dropped = self.counters.dropped.load(Ordering::Relaxed),
            },
            "gateway progress"
        );
    }

    async fn cleanup(&mut self) {
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

fn ended(partial: &str, ending: &str) -> String {
    if partial.is_empty() {
        return ending.to_owned();
    }
    format!("{partial}\n\n{ending}")
}

/// Single-tasked with one writer; nothing but this call's own deadline ever cancels an in-flight
/// call, since a dropped HTTP future cannot retract bytes already sent.
async fn run(
    mut surface: Surface,
    mut events: mpsc::Receiver<ProgressEvent>,
    mut text: watch::Receiver<ModelText>,
    mut terminal: oneshot::Receiver<TerminalRequest>,
    cancellation: SessionCancellation,
) {
    let mut events_open = true;
    let mut text_open = true;
    let coordination = Arc::clone(&surface.coordination);
    loop {
        let wake = surface.next_wake();
        let timer = async move {
            match wake {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            request = &mut terminal => {
                let Ok(request) = request else { break };
                let delivered = surface.terminal(request.terminal).await;
                if request.done.send(delivered).is_err() {
                    tracing::debug!(event = "gateway_progress_terminal_unobserved");
                }
                break;
            }
            () = cancellation.cancelled() => {
                let by = cancellation.source().unwrap_or(CancelSource::Operator);
                surface.terminal(Terminal::Cancelled { by }).await;
                break;
            }
            () = coordination.finished() => break,
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
            () = timer => surface.on_timer().await,
        }
    }
    surface.cleanup().await;
    coordination.finish();
}
