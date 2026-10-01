use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use dekopon_agent::{BrokerLeg, current_trace_parent};
use dekopon_broker_protocol::{BrokerClient, TraceParent, Trigger};
use dekopon_core::AgentId;
use dekopon_process::{CancelHandle, CancelSignal};
use dekopon_shell::{
    CallBudget, ExitCode, Interpreter, JobControl, JobId, JobOutcome, JobRefusal, JobSeed,
    JobState, JobSummary, JobWait, Limits as ShellLimits, TreeContext,
};
use parking_lot::{Condvar, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc};

use crate::{
    config::ResolvedBroker,
    transport::{
        CancelRequest, InboundMessage, MAX_INBOUND_TEXT_BYTES, MessageId, bound_inbound,
        floor_boundary,
    },
    wake::Anchor,
};

const WAIT_SLICE: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JobOwner {
    transport: String,
    conversation: String,
    subject: String,
}

impl JobOwner {
    pub(crate) fn of(message: &InboundMessage) -> Self {
        Self {
            transport: message.transport.clone(),
            conversation: message.conversation.key(),
            subject: message.subject.canonical(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stop {
    Killed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RowState {
    Running,
    Finished {
        outcome: JobOutcome,
        exit: ExitCode,
        ended: Instant,
    },
}

struct Row {
    id: JobId,
    owner: JobOwner,
    text: Arc<str>,
    state: RowState,
    started: Instant,
    cancel: Option<CancelHandle>,
    stop: Option<Stop>,
    waiters: usize,
    waited: bool,
}

impl Row {
    fn stop_running(&mut self) -> bool {
        match self.state {
            RowState::Running => {
                self.stop.get_or_insert(Stop::Cancelled);
                if let Some(handle) = &self.cancel {
                    handle.cancel();
                }
                true
            }
            RowState::Finished { .. } => false,
        }
    }

    fn summary(&self, now: Instant) -> JobSummary {
        JobSummary {
            id: self.id,
            state: match self.state {
                RowState::Running => JobState::Running {
                    elapsed: now.saturating_duration_since(self.started),
                },
                RowState::Finished {
                    outcome,
                    exit,
                    ended,
                } => JobState::Finished {
                    outcome,
                    exit,
                    after: ended.saturating_duration_since(self.started),
                },
            },
            text: Arc::clone(&self.text),
        }
    }
}

struct Table {
    rows: VecDeque<Row>,
    next: u64,
    sealed: bool,
}

impl Table {
    fn owned(&mut self, owner: &JobOwner, id: JobId) -> Result<&mut Row, JobRefusal> {
        self.rows
            .iter_mut()
            .find(|row| row.id == id && row.owner == *owner)
            .ok_or(JobRefusal::NotYours)
    }

    fn evict_finished(&mut self) -> bool {
        let Some(index) = self.rows.iter().position(|row| match row.state {
            RowState::Running => false,
            RowState::Finished { .. } => row.waiters == 0,
        }) else {
            return false;
        };
        self.rows.remove(index);
        true
    }
}

pub(crate) struct Jobs {
    table: Mutex<Table>,
    ended: Condvar,
    permits: Arc<Semaphore>,
    maximum: usize,
    notices: mpsc::Sender<InboundMessage>,
    notice_receiver: Mutex<Option<mpsc::Receiver<InboundMessage>>>,
}

impl Jobs {
    pub(crate) fn new(maximum: usize) -> Self {
        let (notices, receiver) = mpsc::channel(maximum);
        Self {
            table: Mutex::new(Table {
                rows: VecDeque::with_capacity(maximum),
                next: 1,
                sealed: false,
            }),
            ended: Condvar::new(),
            permits: Arc::new(Semaphore::new(maximum)),
            maximum,
            notices,
            notice_receiver: Mutex::new(Some(receiver)),
        }
    }

    pub(crate) fn take_notices(&self) -> mpsc::Receiver<InboundMessage> {
        self.notice_receiver
            .lock()
            .take()
            .expect("one serve loop owns job notices")
    }

    pub(crate) fn admit(
        self: &Arc<Self>,
        owner: JobOwner,
        anchor: Anchor,
        starter: Option<TraceParent>,
        text: Arc<str>,
    ) -> Result<(JobRun, CancelSignal), JobRefusal> {
        let full = JobRefusal::Full {
            maximum: self.maximum,
        };
        let mut table = self.table.lock();
        if table.sealed {
            return Err(JobRefusal::Off);
        }
        let permit = match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits | TryAcquireError::Closed) => return Err(full),
        };
        let (handle, signal) = CancelSignal::pair();
        if table.rows.len() >= self.maximum && !table.evict_finished() {
            return Err(full);
        }
        let id = JobId::new(table.next);
        table.next += 1;
        table.rows.push_back(Row {
            id,
            owner,
            text,
            state: RowState::Running,
            started: Instant::now(),
            cancel: Some(handle),
            stop: None,
            waiters: 0,
            waited: false,
        });
        drop(table);
        Ok((
            JobRun {
                jobs: Arc::clone(self),
                id,
                anchor,
                starter,
                permit: Some(permit),
                completed_trace: None,
                completed_span: None,
            },
            signal,
        ))
    }

    /// Every ending passes through here exactly once, and the permit is released only after the
    /// row is final, so a freed slot never finds its predecessor still `running`.
    fn finish(&self, run: &JobRun, ended: Ended, permit: OwnedSemaphorePermit) {
        let id = run.id;
        let mut table = self.table.lock();
        let finished = table.rows.iter_mut().find(|row| row.id == id).map(|row| {
            let outcome = match (row.stop, ended.ending) {
                (Some(Stop::Killed), _) => JobOutcome::Killed,
                (Some(Stop::Cancelled), _) => JobOutcome::Cancelled,
                (None, Ending::Abandoned) => JobOutcome::Failed,
                (None, Ending::Ran { .. }) if ended.exit == ExitCode::SUCCESS => {
                    JobOutcome::Succeeded
                }
                (None, Ending::Ran { expired: true }) if ended.exit == ExitCode::TIMEOUT => {
                    JobOutcome::Deadline
                }
                (None, Ending::Ran { .. }) => JobOutcome::Failed,
            };
            let now = Instant::now();
            row.state = RowState::Finished {
                outcome,
                exit: ended.exit,
                ended: now,
            };
            row.cancel = None;
            row.waited |= row.waiters > 0;
            (
                outcome,
                row.waited,
                Arc::clone(&row.text),
                now.saturating_duration_since(row.started),
            )
        });
        drop(table);
        self.ended.notify_all();
        if let Some((outcome, waited, text, after)) = finished {
            let record = || {
                tracing::info!(name: "job.finished", target: "job", {
                        job.id = %id,
                        job.outcome = outcome.as_str(),
                        job.exit_code = ended.exit.get(),
                        agent = %run.anchor.agent(),
                        conversation.id = %run.anchor.conversation().key(),
                        subject = %run.anchor.subject().canonical(),
                        duration_ms = after.as_millis() as u64,
                    }, "job.finished");
            };
            if let Some(span) = &run.completed_span {
                span.in_scope(record);
            } else {
                record();
            }
            tracing::debug!(
                event = "gateway_job_finished",
                job.id = id.get(),
                agent = %run.anchor.agent(),
                job.outcome = outcome.as_str(),
                job.exit_code = ended.exit.get(),
                job.waited = waited,
                job.output.bytes = ended.output.len(),
            );
            let noticed = match outcome {
                JobOutcome::Succeeded | JobOutcome::Failed | JobOutcome::Deadline => !waited,
                JobOutcome::Cancelled | JobOutcome::Killed => false,
            };
            if noticed {
                let body = format!(
                    "[gateway: job {id} finished, exit {}, after {}s]\n{text}\n{}",
                    ended.exit.get(),
                    after.as_secs(),
                    ended.output
                );
                let mut bounded = bound_inbound(&body);
                if bounded.len() > MAX_INBOUND_TEXT_BYTES {
                    bounded.truncate(floor_boundary(&bounded, MAX_INBOUND_TEXT_BYTES));
                }
                let message = run.anchor.job_inbound(id, bounded, run.completed_trace);
                if let Err(error) = self.notices.try_send(message)
                    && let Some(notice) = Notice::of(&error.into_inner())
                {
                    notice.record("dropped");
                }
            }
        }
        drop(permit);
    }

    #[cfg(test)]
    pub(crate) fn free_permits(&self) -> usize {
        self.permits.available_permits()
    }

    pub(crate) fn list(&self, owner: &JobOwner) -> Vec<JobSummary> {
        let now = Instant::now();
        self.table
            .lock()
            .rows
            .iter()
            .filter(|row| row.owner == *owner)
            .map(|row| row.summary(now))
            .collect()
    }

    pub(crate) fn wait(
        &self,
        owner: &JobOwner,
        ids: &[JobId],
        keep_waiting: &dyn Fn() -> bool,
    ) -> Vec<Result<JobWait, JobRefusal>> {
        let mut answers = Vec::with_capacity(ids.len());
        let mut held = Vec::with_capacity(ids.len());
        let mut table = self.table.lock();
        for &id in ids {
            match table.owned(owner, id) {
                Ok(row) => {
                    row.waiters += 1;
                    held.push(id);
                    answers.push(None);
                }
                Err(refusal) => answers.push(Some(Err(refusal))),
            }
        }
        drop(table);
        let _waiters = Waiters {
            jobs: self,
            owner,
            ids: held,
        };
        let mut table = self.table.lock();
        loop {
            let mut pending = false;
            for (answer, &id) in answers.iter_mut().zip(ids) {
                if answer.is_some() {
                    continue;
                }
                match table.owned(owner, id) {
                    Ok(row) => match row.state {
                        RowState::Finished { exit, .. } => {
                            row.waited = true;
                            *answer = Some(Ok(JobWait::Exited(exit)));
                        }
                        RowState::Running => pending = true,
                    },
                    Err(refusal) => *answer = Some(Err(refusal)),
                }
            }
            if !pending {
                break;
            }
            if !parking_lot::MutexGuard::unlocked(&mut table, keep_waiting) {
                break;
            }
            self.ended.wait_for(&mut table, WAIT_SLICE);
        }
        drop(table);
        answers
            .into_iter()
            .map(|answer| answer.unwrap_or(Ok(JobWait::Interrupted)))
            .collect()
    }

    pub(crate) fn cancel_owner(&self, request: &CancelRequest) -> bool {
        let mut table = self.table.lock();
        let mut stopped = false;
        for row in &mut table.rows {
            if row.owner.transport == request.transport
                && row.owner.conversation == request.conversation_id
                && row.owner.subject == request.subject
            {
                stopped |= row.stop_running();
            }
        }
        stopped
    }

    pub(crate) fn seal(&self) {
        let mut table = self.table.lock();
        table.sealed = true;
        for row in &mut table.rows {
            row.stop_running();
        }
    }

    pub(crate) fn kill(&self, owner: &JobOwner, id: JobId) -> Result<(), JobRefusal> {
        let mut table = self.table.lock();
        let row = table.owned(owner, id)?;
        match row.state {
            RowState::Finished { .. } => Err(JobRefusal::Finished),
            RowState::Running => {
                row.stop.get_or_insert(Stop::Killed);
                if let Some(handle) = &row.cancel {
                    handle.cancel();
                }
                Ok(())
            }
        }
    }
}

pub(crate) struct Notice {
    id: JobId,
    agent: AgentId,
    owner: JobOwner,
}

impl Notice {
    pub(crate) fn of(message: &InboundMessage) -> Option<Self> {
        match &message.message_id {
            MessageId::Job { id, agent } => Some(Self {
                id: *id,
                agent: agent.clone(),
                owner: JobOwner::of(message),
            }),
            MessageId::Native(_) | MessageId::Wake { .. } => None,
        }
    }

    pub(crate) fn record(&self, delivery: &'static str) {
        tracing::info!(name: "job.notice", target: "job", {
            job.id = %self.id,
            job.notice.delivery = delivery,
            agent = %self.agent,
            conversation.id = %self.owner.conversation,
            subject = %self.owner.subject,
        }, "job.notice");
    }
}

struct Waiters<'a> {
    jobs: &'a Jobs,
    owner: &'a JobOwner,
    ids: Vec<JobId>,
}

impl Drop for Waiters<'_> {
    fn drop(&mut self) {
        let mut table = self.jobs.table.lock();
        for &id in &self.ids {
            if let Ok(row) = table.owned(self.owner, id) {
                row.waiters -= 1;
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Ending {
    Ran { expired: bool },
    Abandoned,
}

#[derive(Debug)]
struct Ended {
    exit: ExitCode,
    output: String,
    ending: Ending,
}

/// Owns the row from admission to its one `finish`; dropped unfinished, it fails the row so a
/// panicking or never-spawned job cannot leave it `running` or hold its permit.
pub(crate) struct JobRun {
    jobs: Arc<Jobs>,
    id: JobId,
    anchor: Anchor,
    starter: Option<TraceParent>,
    permit: Option<OwnedSemaphorePermit>,
    completed_trace: Option<TraceParent>,
    completed_span: Option<tracing::Span>,
}

impl JobRun {
    fn finish_with_trace(mut self, ended: Ended, trace: Option<TraceParent>, span: tracing::Span) {
        self.completed_trace = trace;
        self.completed_span = Some(span);
        if let Some(permit) = self.permit.take() {
            self.jobs.finish(&self, ended, permit);
        }
    }
}

impl Drop for JobRun {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            Arc::clone(&self.jobs).finish(
                self,
                Ended {
                    exit: ExitCode::FAILURE,
                    output: "[gateway: the job stopped before it could finish]".to_owned(),
                    ending: Ending::Abandoned,
                },
                permit,
            );
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Origin {
    Turn,
    Tree(Trigger),
}

#[derive(Clone)]
pub(crate) struct JobContext {
    jobs: Arc<Jobs>,
    owner: JobOwner,
    anchor: Anchor,
    broker: ResolvedBroker,
    runtime: tokio::runtime::Handle,
    limits: ShellLimits,
    origin: Origin,
}

impl JobContext {
    pub(crate) fn for_turn(
        jobs: Arc<Jobs>,
        message: &InboundMessage,
        anchor: Anchor,
        broker: ResolvedBroker,
        limits: ShellLimits,
    ) -> Self {
        Self {
            jobs,
            owner: JobOwner::of(message),
            anchor,
            broker,
            runtime: tokio::runtime::Handle::current(),
            limits,
            origin: Origin::Turn,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_probe(self) -> Self {
        Self {
            origin: Origin::Tree(Trigger::Probe),
            ..self
        }
    }

    pub(crate) fn from_probe(
        jobs: Arc<Jobs>,
        anchor: Anchor,
        broker: ResolvedBroker,
        runtime: tokio::runtime::Handle,
        limits: ShellLimits,
    ) -> Self {
        let owner = JobOwner {
            transport: anchor.transport().to_string(),
            conversation: anchor.conversation().key(),
            subject: anchor.subject().canonical(),
        };
        Self {
            jobs,
            owner,
            anchor,
            broker,
            runtime,
            limits,
            origin: Origin::Tree(Trigger::Probe),
        }
    }

    const fn trigger(&self) -> Trigger {
        match self.origin {
            Origin::Turn => Trigger::Job,
            Origin::Tree(trigger) => trigger,
        }
    }

    fn connect(&self, signal: CancelSignal) -> Option<BrokerLeg> {
        let client = BrokerClient::new(
            &self.broker.socket_path,
            self.broker.server_uid,
            self.broker.frame,
        )
        .inspect_err(|error| {
            tracing::warn!(event = "gateway_job_leg_unavailable", error = %error);
        })
        .ok()?;
        let leg = self
            .runtime
            .block_on(BrokerLeg::connect(
                client,
                Some(self.anchor.claim(self.trigger())),
            ))
            .inspect_err(|error| {
                tracing::warn!(event = "gateway_job_leg_unavailable", error = %error);
            })
            .ok()?;
        let nested = Self {
            origin: Origin::Tree(self.trigger()),
            ..self.clone()
        };
        Some(
            leg.with_cancel_signal(signal)
                .with_job_control(Arc::new(nested)),
        )
    }

    fn tree(&self, seed: &JobSeed) -> TreeContext {
        match self.origin {
            Origin::Turn => TreeContext::new(
                self.limits,
                CallBudget::new(self.limits.max_capability_calls),
            ),
            Origin::Tree(_) => seed.tree().clone(),
        }
    }

    fn run(&self, seed: JobSeed, tree: &TreeContext, signal: CancelSignal) -> Ended {
        let Some(leg) = self.connect(signal) else {
            return Ended {
                exit: ExitCode::FAILURE,
                output: "[gateway: the broker could not be reached to run the job]".to_owned(),
                ending: Ending::Ran { expired: false },
            };
        };
        let outcome = Interpreter::new(self.limits).run_seed(seed, &leg, tree);
        Ended {
            exit: outcome.exit_code,
            output: outcome.output,
            ending: Ending::Ran {
                expired: tree.expired(),
            },
        }
    }
}

struct Started {
    context: JobContext,
    run: JobRun,
    seed: JobSeed,
    tree: TreeContext,
    signal: CancelSignal,
}

impl Started {
    fn run(self) {
        let Self {
            context,
            run,
            seed,
            tree,
            signal,
        } = self;
        let span = tracing::info_span!(
            target: "job",
            parent: None,
            "gateway.job",
            job.id = run.id.get(),
        );
        if let Some(parent) = run.starter {
            dekopon_telemetry::link_remote(
                &span,
                dekopon_telemetry::TraceContextParts {
                    trace_id: parent.trace_id(),
                    span_id: parent.parent_id(),
                    flags: parent.flags(),
                },
            );
        }
        let parent = span.in_scope(current_trace_parent);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            span.in_scope(|| context.run(seed, &tree, signal))
        }));
        match result {
            Ok(ended) => run.finish_with_trace(ended, parent, span),
            Err(panic) => {
                run.finish_with_trace(
                    Ended {
                        exit: ExitCode::FAILURE,
                        output: "[gateway: the job stopped before it could finish]".to_owned(),
                        ending: Ending::Abandoned,
                    },
                    parent,
                    span,
                );
                std::panic::resume_unwind(panic);
            }
        }
    }
}

impl JobControl for JobContext {
    fn start(&self, seed: JobSeed) -> Result<JobId, JobRefusal> {
        let (run, signal) = self.jobs.admit(
            self.owner.clone(),
            self.anchor.clone(),
            current_trace_parent(),
            Arc::clone(seed.text()),
        )?;
        let id = run.id;
        let tree = self.tree(&seed);
        let text = seed.text();
        tracing::info!(name: "job.started", target: "job", {
            job.id = %id,
            job.deadline_ms = tree.remaining().as_millis() as u64,
            job.script.head = %dekopon_core::bounded_display(text).text(),
            job.script.bytes = text.len(),
            agent = %self.anchor.agent(),
            conversation.id = %self.anchor.conversation().key(),
            subject = %self.anchor.subject().canonical(),
        }, "job.started");
        let started = Started {
            context: self.clone(),
            run,
            seed,
            tree,
            signal,
        };
        let _runtime = self.runtime.enter();
        let subscriber = tracing::dispatcher::get_default(Clone::clone);
        drop(tokio::task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&subscriber, || started.run());
        }));
        Ok(id)
    }

    fn list(&self) -> Vec<JobSummary> {
        self.jobs.list(&self.owner)
    }

    fn wait(
        &self,
        ids: &[JobId],
        keep_waiting: &dyn Fn() -> bool,
    ) -> Vec<Result<JobWait, JobRefusal>> {
        self.jobs.wait(&self.owner, ids, keep_waiting)
    }

    fn kill(&self, id: JobId) -> Result<(), JobRefusal> {
        self.jobs.kill(&self.owner, id)
    }
}
