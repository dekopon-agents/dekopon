//! One session's progress task: what the waiting person is shown, and when.
//!
//! The policy owns timing, budgets, and the single message the gateway may edit; the driver owns
//! credentials, endpoints, and what each service will accept. Everything here is presentation and
//! none of it can fail a session: a refused edit is a debug record, two consecutive refusals stop
//! that rung for the session — one, when the call that missed its deadline was the one creating
//! the surface — and the answer goes out either way.
//!
//! It is also the **only** terminal writer once a session has started. A cancelled session's
//! `Stopped.` used to come from the routing loop while this task owned the message being edited,
//! which is two tasks writing one conversation with no ordering between them. One writer removes
//! the race rather than sequencing it.

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
    task::JoinHandle,
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

/// Stop rendering on one surface after this many consecutive failures.
///
/// A later session tries the installation again: the rung is off for this session rather than for
/// the process, because the usual cause is one message that cannot be edited rather than a
/// permanently missing scope.
///
/// The exception is a call that was creating the surface and missed its deadline, which stops the
/// rung at one: see [`Breaker::orphaned`].
const MAX_CONSECUTIVE_FAILURES: u8 = 2;
/// Edits one session may spend on its progress message.
///
/// The guard against a pathological event stream, not against rate limits: those are the
/// transport's `min_edit_interval` and its own 429 cooldown, which are shared across sessions and
/// are the only thing that can see another session's traffic.
const MAX_EDITS: u32 = 60;
/// How long this task waits on one service call before it stops waiting.
///
/// A hung endpoint is a different failure from a slow one: a service that accepts the connection
/// and then answers nothing holds this task for as long as it holds the socket, and this task is
/// single-tasked on purpose, so the person's answer waits behind a typing indicator. Two seconds
/// is longer than any of these calls takes when the service is working at all.
pub(super) const CALL_DEADLINE: Duration = Duration::from_secs(2);
/// The category a call that ran out its deadline reports, beside `TransportError::category`.
const DEADLINE_MISSED: &str = "deadline";

/// Awaits one service call under this task's own deadline.
///
/// The deadline is the only thing that ever cancels a call in flight: nothing here drops a call
/// because a cancel arrived or the session ended, because a dropped request cannot retract the
/// bytes already sent. A call that misses it is that rung's failure, so two in a row trip the rung
/// for the session and the answer goes out either way.
///
/// A miss is weaker news than a refusal, and every caller that can create or replace a message
/// acts on the difference: the transport still holds the call, so the effect may land after this
/// task stopped waiting. [`Breaker::orphaned`] and the deadline branch in [`Surface::finalize`]
/// are where that is spent.
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

/// How a session ends, from the point of view of the one message on screen.
#[derive(Debug)]
pub(crate) enum Terminal {
    /// The session's bounded answer, which the surface becomes in place when it can.
    Answered(OutboundReply),
    /// The session failed; a progress message is removed and this one fixed sentence is the whole
    /// reply, while a stream gains the sentence under what it already showed.
    Failed(String),
    /// The session was stopped; whatever is on screen keeps its place and gains the stopped line.
    Cancelled { by: CancelSource },
    /// The session deliberately said nothing; nothing is posted and the surface stops claiming a
    /// run is under way.
    ///
    /// Its own ending rather than an empty answer: a surface left saying "Working on it…" is a
    /// claim about a session that has ended, and posting "" to say so would be a reply the
    /// continuation deliberately declined to make. A progress message is removed; a stream cannot
    /// be, so it is closed on exactly the text the person had already read.
    Silent,
}

struct TerminalRequest {
    terminal: Terminal,
    done: oneshot::Sender<bool>,
}

/// RUNNING → SEALED → FINISHED, shared between the session and its policy task.
#[derive(Default)]
struct Coordination {
    state: AtomicU8,
    changed: Notify,
}

impl Coordination {
    /// Stops new renewals and edits without waiting on remote cosmetic I/O.
    ///
    /// Sealing a generation that already reached SEALED or FINISHED is the no-op it looks like: a
    /// cancel and the session's own completion race, and the later one must not reopen.
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

    /// Resolves once the owning session is gone, so an aborted session's task stops rendering.
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

/// Everything one session's policy task needs, gathered where a session builds it.
pub(crate) struct ProgressInputs {
    pub driver: Arc<dyn ChatDriver>,
    /// Authenticated coordinates for the transient surfaces, absent when the transport has none.
    pub target: Option<LivenessTarget>,
    /// Where a reply goes when there is no surface to finalize.
    pub reply: ReplyTarget,
    pub transport: String,
    pub detail: ProgressDetail,
    /// The transport's block, for the operator wording it owns.
    pub liveness: Arc<ResolvedLiveness>,
    /// What this conversation kind actually renders, and how often it says it is alive.
    ///
    /// Resolved by the session from `ResolvedLiveness::for_kind`, because a direct message with
    /// one reader and a channel with a hundred are worth different budgets from one block.
    pub settings: LivenessSettings,
    pub keep_alive: KeepAlive,
    pub cancellation: SessionCancellation,
    /// Wall-clock bound counted from `Started`, which is agent time rather than admission time.
    pub max_duration: Option<Duration>,
}

/// The session's handle on its policy task.
pub(crate) struct ProgressPolicy {
    coordination: Arc<Coordination>,
    terminal: Option<oneshot::Sender<TerminalRequest>>,
    worker: Option<JoinHandle<()>>,
}

impl ProgressPolicy {
    /// Starts one session's policy task and hands back the sink the prompt loop emits into.
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
        // Inside the caller's span, so every record this task writes rides the message's trace
        // rather than opening an orphan the operator cannot tie to a conversation.
        let worker = tokio::spawn(tracing::Instrument::instrument(
            run(surface, events_rx, text_rx, terminal_rx, cancellation),
            tracing::Span::current(),
        ));
        (
            Self {
                coordination,
                terminal: Some(terminal_tx),
                worker: Some(worker),
            },
            adapter,
        )
    }

    /// Synchronously stops renewals and edits, without waiting on remote cosmetic I/O.
    pub(crate) fn seal(&self) {
        self.coordination.seal();
    }

    /// Hands the session's ending to the one task that owns the surface, and reports whether the
    /// person was told.
    ///
    /// `false` means no acceptance receipt: either the transport refused the reply, or the task
    /// had already written the ending because a cancel reached it first.
    pub(crate) async fn terminal(&mut self, terminal: Terminal) -> bool {
        let Some(sender) = self.terminal.take() else {
            return false;
        };
        let (done, wait) = oneshot::channel();
        if sender.send(TerminalRequest { terminal, done }).is_err() {
            // The task already ended the session, which is the cancel path having won.
            return false;
        }
        wait.await.unwrap_or(false)
    }

    /// Lets service-specific cleanup follow terminal delivery without holding admission.
    pub(crate) fn finish_in_background(&mut self) {
        // Dropping a Tokio JoinHandle detaches rather than aborts: the task owns its own bounded
        // calls and exits after cleanup while the answered session releases its permit now.
        self.worker.take();
    }
}

impl Drop for ProgressPolicy {
    fn drop(&mut self) {
        self.coordination.finish();
    }
}

/// One rung of the ladder, and whether it is still worth trying.
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

    /// A call that may have created a surface this session cannot name, which stops the rung now.
    ///
    /// Only a deadline reaches this. An error is the service having answered "no" and left nothing
    /// behind, but a deadline is this task giving up on a call the transport still holds: a `post`
    /// or a first `show` that lands a second later leaves a message on screen with no `MessageRef`
    /// here to edit, stream into, or finalize. Trying again would post a *second* message beside
    /// it and finalize only that one, so one failure is the whole budget for a creating call and
    /// the answer goes out as an ordinary reply instead.
    fn orphaned(&mut self, transport: &str, primitive: &'static str) {
        self.observed(transport, primitive, DEADLINE_MISSED, 1);
    }

    /// Records one failed call and trips the rung at `ceiling` consecutive failures.
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

/// Which line one render writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Line {
    /// The verb for what the session is doing right now.
    Status,
    /// The tick that says the session is still alive, with a number that is fresh by construction.
    KeepAlive,
}

/// The one message on screen, plus everything that decides when it changes.
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
    /// The single message this session may edit, stream into, finalize, or delete.
    message: Option<MessageRef>,
    /// Whether that message is the stream rather than the progress line.
    streaming: bool,
    latest_text: Option<ModelText>,
    /// Whether the first text has already been spent as a post trigger; see [`Surface::on_text`].
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
        }
    }

    /// Whether the transport publishes anything at all while a session runs.
    fn native(&self) -> bool {
        self.settings.mode == LivenessMode::Native && self.target.is_some()
    }

    /// Whether the streamed answer is this session's surface.
    ///
    /// When it is, no separate progress message is ever posted: one `MessageRef` per session is
    /// what keeps "which message does the answer land in" a question with one answer, and Slack's
    /// append-only stream cannot be the second message beside a status line.
    fn streams(&self) -> bool {
        self.native()
            && self.detail != ProgressDetail::Off
            && self.settings.stream
            && self.driver.stream().is_some()
    }

    /// Whether a progress message may be posted or edited.
    fn writes_progress(&self) -> bool {
        self.native()
            && self.detail != ProgressDetail::Off
            && self.settings.progress == ProgressSurface::Message
            && !self.streams()
    }

    /// Whether the surface carries the transport's own stop control.
    fn cancel_control(&self) -> bool {
        self.settings.cancel_button && self.driver.cancel_button().is_some()
    }

    /// The next instant this task has something to do.
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
                // Deliberately no post: a fast one-turn answer must not leave a "Working on it…"
                // message behind the reply that arrived a second later.
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
            // The loop's own view of the ending. The session hands the policy its terminal
            // separately, carrying the text only it has, so acting on these would write twice.
            ProgressEvent::TextDelta { .. }
            | ProgressEvent::KeepAlive { .. }
            | ProgressEvent::Cancelled { .. }
            | ProgressEvent::Failed { .. }
            | ProgressEvent::Finished { .. } => {}
        }
    }

    /// The t=0 ladder: the cheapest signal first, so something changes before the model answers.
    async fn open(&mut self) {
        if !self.native() {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let driver = Arc::clone(&self.driver);
        if let Some(reaction) = driver.reaction() {
            let outcome = bounded(reaction.set(&target, true)).await;
            self.observe(outcome, "reaction");
        }
        if let Some(typing) = driver.typing() {
            let outcome = bounded(typing.renew(&target)).await;
            self.observe(outcome, "typing");
            self.next_typing = Some(Instant::now() + typing.renew_every());
        }
        if let Some(status) = driver.status() {
            let outcome = bounded(status.set(&target, Status::Working)).await;
            self.observe(outcome, "status");
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
        // Streaming off, so the text itself never reaches the surface — but its arrival is the
        // first proof the model is writing rather than thinking, and it is a post trigger beside
        // a capability call and the first keep-alive tick. Without this a long streamed turn with
        // no tool call leaves the person looking at nothing until the 15 s tick, which is the
        // common shape: `stream` is off by default and every shipped example configures it so.
        //
        // Once, and only to post: the flag is spent even when the post is refused, so a turn's
        // hundreds of deltas cannot become one attempt per delta, and a surface that already
        // exists is left alone because text is not what a progress line shows.
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
            // Fired once: the cancel is a compare-exchange, and the render follows from the
            // notification it raises rather than from here.
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
            // The first tick is also the last chance for a slow first turn to say anything, so it
            // may post where an ordinary edit may not.
            self.render(Line::KeepAlive, true).await;
        }
        // Cleared before the attempt, not inside it: a render the policy declines to make — a
        // sealed session, a tripped rung — must not leave a deadline in the past for this loop to
        // wake on again immediately.
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
        self.observe(outcome, "typing");
        self.next_typing = self
            .breakers
            .typing
            .allows()
            .then(|| Instant::now() + typing.renew_every());
    }

    /// When the next keep-alive tick is due, or `None` once the budget is spent.
    ///
    /// Measured from the tick before it — `from` is the session's start for the first one and the
    /// instant the last one fired thereafter — rather than from a table of offsets against
    /// `Started`. A task that woke late, because a service call took its whole deadline or the
    /// runtime was busy, would otherwise find every offset it slept through already due and fire
    /// them into one instant, where the coalescing window folds them into a single edit that spent
    /// the whole budget. Ten "still working" lines in the same second say no more than one, and
    /// each line still reads its elapsed seconds off the clock, so a late tick tells the truth
    /// about how long the person has been waiting.
    fn schedule_keep_alive(&mut self, from: Instant) {
        let keep_alive = &self.keep_alive;
        if self.keep_alives >= keep_alive.max {
            self.next_keep_alive = None;
            return;
        }
        let fired = self.keep_alives as usize;
        let gap = match keep_alive.at.get(fired) {
            // Inside the opening schedule, where the gap is this offset less the one before it.
            Some(offset) => {
                let previous = fired
                    .checked_sub(1)
                    .and_then(|index| keep_alive.at.get(index))
                    .copied()
                    .unwrap_or_default();
                offset.saturating_sub(previous)
            }
            // Past the end of it, where the cadence is one period.
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
            // Coalesced rather than queued: the next render shows the newest state, not a backlog
            // of the states in between.
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
        // Whether this call is the one that creates the surface, which decides what a deadline
        // miss means below.
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
        // Every path that will not render clears the owed flag, so a deadline already in the past
        // cannot make this task's timer wake it again and again.
        if !self.streams() || !self.coordination.running() || !self.breakers.stream.allows() {
            self.stream_pending = false;
            return;
        }
        let now = Instant::now();
        if let Some(next) = self.next_stream
            && now < next
        {
            // The only early return that keeps the flag: the deadline is in the future, which is
            // what the task's next wake is computed from.
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
        // The first `show` is what creates the streamed message; every later one appends to it.
        let creating = self.message.is_none();
        let outcome = bounded(stream.show(&target, self.message.as_ref(), &text, cancel)).await;
        self.stream_pending = false;
        self.next_stream = Some(Instant::now() + limits.min_interval);
        match outcome {
            Ok(message) => {
                self.breakers.stream.succeeded();
                self.message = Some(message);
                self.streaming = true;
                // `chars` is what was on screen, which is not the same as what the model had
                // written: rendering lags by one interval and stops at the surface's ceiling.
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

    /// Writes the session's ending, exactly once, and reports whether it was accepted.
    async fn terminal(&mut self, terminal: Terminal) -> bool {
        self.coordination.seal();
        self.record_terminal(&terminal);
        // What a streamed surface already shows, when the surface is a stream at all. An
        // append-only stream is the reason this is asked once for every ending rather than only
        // for a cancel: `discard` cannot remove one and blanking it would take back what the
        // person already read, so every ending that would have deleted a progress message closes
        // the stream in place instead, keeping this text above it. Left open, the stream stays
        // live on the service and the driver's registry entry is never taken.
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
                    // Closed on exactly what is already on screen: the stream has to end — an open
                    // one claims the session is still writing — but a silent session adds no
                    // sentence to it, and the fallback reply repeats text the person has read
                    // rather than making the reply the continuation declined to make.
                    Some(partial) => {
                        self.finalize(OutboundReply::text(partial)).await;
                    }
                    None => self.discard().await,
                }
                false
            }
        }
    }

    /// The model text a streamed surface is showing, or `None` when the surface is not a stream.
    fn streamed_text(&self) -> Option<String> {
        self.streaming.then(|| {
            self.latest_text
                .as_ref()
                .map(ModelText::as_str)
                .unwrap_or_default()
                .to_owned()
        })
    }

    /// Turns the surface into the ending in place, falling back to removing it and replying.
    ///
    /// The fallback is not a retry: `finalize` failing means this message cannot become the
    /// answer, and a message nobody can read is worse than a second post.
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
                    // A finalize that ran out its deadline may have landed. `discard` would then
                    // delete the message that already carries the answer, and a person cannot read
                    // a message that is gone — where they can read a second copy of one. So a
                    // deadline miss leaves whatever is on screen alone and posts the answer beside
                    // it; only a refusal, which is the service saying the edit did not happen,
                    // still removes the surface first.
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

    /// Removes the surface, because what it says is no longer true.
    async fn discard(&mut self) {
        let Some(message) = self.message.take() else {
            return;
        };
        // An append-only stream has no delete, and blanking the partial text would take back what
        // the person already read; the fallback reply carries the ending instead.
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

    /// The one place the session's own ending reaches the trace, with what the surface counted.
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

    /// Returns the service's own indicators to rest, after the ending is on screen.
    async fn cleanup(&mut self) {
        if !self.native() {
            return;
        }
        let Some(target) = self.target.clone() else {
            return;
        };
        let driver = Arc::clone(&self.driver);
        if let Some(status) = driver.status() {
            let outcome = bounded(status.set(&target, Status::Idle)).await;
            self.observe(outcome, "status");
        }
        if let Some(reaction) = driver.reaction() {
            let outcome = bounded(reaction.set(&target, false)).await;
            self.observe(outcome, "reaction");
        }
    }

    /// Records one rung's call and trips its breaker after two consecutive failures.
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

/// One ending as it reads under a streamed partial answer.
///
/// A blank line, because the two are different voices: everything above it is the model's, and the
/// sentence below it is the daemon's. A stream that had shown nothing yet is the sentence alone.
fn ended(partial: &str, ending: &str) -> String {
    if partial.is_empty() {
        return ending.to_owned();
    }
    format!("{partial}\n\n{ending}")
}

/// One session's policy task.
///
/// Single-tasked on purpose: one message, one writer, and an issued service call that no other
/// event cancels, because a dropped HTTP future cannot retract bytes already sent and the
/// reordering that follows is exactly the "Stopped." before the partial text this design removes.
/// Each call's own [`CALL_DEADLINE`] is the single exception, and the reason a hung endpoint costs
/// the waiting person two seconds rather than the whole answer.
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
                // Rendered here rather than when the session task unwinds: a stop pressed while the
                // model is mid-request has to change the screen now, not after the request returns.
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
