use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use dekopon_agent::{BrokerLeg, current_trace_parent};
use dekopon_broker_protocol::{BrokerClient, TraceParent, Trigger};
use dekopon_process::{CancelHandle, CancelSignal};
use dekopon_shell::{
    CallBudget, ExitCode, Interpreter, JobControl, JobId, JobOutcome, JobRefusal, JobSeed,
    JobState, JobSummary, JobWait, Limits as ShellLimits, TreeContext,
};
use parking_lot::{Condvar, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::{
    config::ResolvedBroker,
    transport::{CancelRequest, InboundMessage},
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
}

impl Jobs {
    pub(crate) fn new(maximum: usize) -> Self {
        Self {
            table: Mutex::new(Table {
                rows: VecDeque::with_capacity(maximum),
                next: 1,
            }),
            ended: Condvar::new(),
            permits: Arc::new(Semaphore::new(maximum)),
            maximum,
        }
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
        let permit = match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits | TryAcquireError::Closed) => return Err(full),
        };
        let (handle, signal) = CancelSignal::pair();
        let mut table = self.table.lock();
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
            row.state = RowState::Finished {
                outcome,
                exit: ended.exit,
                ended: Instant::now(),
            };
            row.cancel = None;
            row.waited |= row.waiters > 0;
            (outcome, row.waited)
        });
        drop(table);
        self.ended.notify_all();
        if let Some((outcome, waited)) = finished {
            tracing::debug!(
                event = "gateway_job_finished",
                job.id = id.get(),
                agent = %run.anchor.agent(),
                job.outcome = outcome.as_str(),
                job.exit_code = ended.exit.get(),
                job.waited = waited,
                job.output.bytes = ended.output.len(),
            );
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
        id: JobId,
        keep_waiting: &dyn Fn() -> bool,
    ) -> Result<JobWait, JobRefusal> {
        let mut table = self.table.lock();
        table.owned(owner, id)?.waiters += 1;
        drop(table);
        let _waiter = Waiter {
            jobs: self,
            owner,
            id,
        };
        let mut table = self.table.lock();
        loop {
            let row = table.owned(owner, id)?;
            if let RowState::Finished { exit, .. } = row.state {
                row.waited = true;
                return Ok(JobWait::Exited(exit));
            }
            if !parking_lot::MutexGuard::unlocked(&mut table, keep_waiting) {
                return Ok(JobWait::Interrupted);
            }
            self.ended.wait_for(&mut table, WAIT_SLICE);
        }
    }

    pub(crate) fn cancel_owner(&self, request: &CancelRequest) -> bool {
        let mut table = self.table.lock();
        let mut stopped = false;
        for row in &mut table.rows {
            if row.owner.transport == request.transport
                && row.owner.conversation == request.conversation_id
                && row.owner.subject == request.subject
                && matches!(row.state, RowState::Running)
            {
                row.stop.get_or_insert(Stop::Cancelled);
                if let Some(handle) = &row.cancel {
                    handle.cancel();
                }
                stopped = true;
            }
        }
        stopped
    }

    pub(crate) fn cancel_all(&self) {
        let mut table = self.table.lock();
        for row in &mut table.rows {
            if matches!(row.state, RowState::Running) {
                row.stop.get_or_insert(Stop::Cancelled);
                if let Some(handle) = &row.cancel {
                    handle.cancel();
                }
            }
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

struct Waiter<'a> {
    jobs: &'a Jobs,
    owner: &'a JobOwner,
    id: JobId,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        if let Ok(row) = self.jobs.table.lock().owned(self.owner, self.id) {
            row.waiters -= 1;
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
}

impl JobRun {
    fn finish(mut self, ended: Ended) {
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

    fn run(&self, seed: JobSeed, signal: CancelSignal) -> Ended {
        let Some(leg) = self.connect(signal) else {
            return Ended {
                exit: ExitCode::FAILURE,
                output: "[gateway: the broker could not be reached to run the job]".to_owned(),
                ending: Ending::Ran { expired: false },
            };
        };
        let tree = match self.origin {
            Origin::Turn => TreeContext::new(
                self.limits,
                CallBudget::new(self.limits.max_capability_calls),
            ),
            Origin::Tree(_) => seed.tree().clone(),
        };
        let outcome = Interpreter::new(self.limits).run_seed(seed, &leg, &tree);
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
    signal: CancelSignal,
}

impl Started {
    fn run(self) {
        let Self {
            context,
            run,
            seed,
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
        let ended = span.in_scope(|| context.run(seed, signal));
        drop(span);
        run.finish(ended);
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
        let started = Started {
            context: self.clone(),
            run,
            seed,
            signal,
        };
        let _runtime = self.runtime.enter();
        drop(tokio::task::spawn_blocking(move || started.run()));
        Ok(id)
    }

    fn list(&self) -> Vec<JobSummary> {
        self.jobs.list(&self.owner)
    }

    fn wait(&self, id: JobId, keep_waiting: &dyn Fn() -> bool) -> Result<JobWait, JobRefusal> {
        self.jobs.wait(&self.owner, id, keep_waiting)
    }

    fn kill(&self, id: JobId) -> Result<(), JobRefusal> {
        self.jobs.kill(&self.owner, id)
    }
}
