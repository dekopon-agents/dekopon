//! Joined Tokio process lifecycle for unprivileged Dekopon frontends.
//!
//! This slice owns exactly one boundary: run one asynchronous operation in a traced Tokio task and
//! join it before returning. A process is either non-interruptible or cancellable. Cancellation is
//! cooperative and minimal: a [`CancelHandle`] asks, the supervisor aborts the node's Tokio task at
//! its next `.await`, and the supervisor still joins that task before it reports anything. The
//! supervisor joins the node's own Tokio task and nothing else: work the node handed to
//! [`tokio::task::spawn_blocking`] or spawned as another task is detached by the abort, is not
//! joined, and can outlive a `cancelled` outcome. A node that must not leave such work behind
//! must stay [`ProcessMetadata::non_interruptible`]. Structured process trees, ports, deadlines,
//! and graph scheduling remain deferred until a production frontend consumes them.

#![forbid(unsafe_code)]

use std::{
    error::Error,
    fmt,
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use tokio::sync::watch;
use tracing::Instrument as _;

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct RunId(u64);

impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy)]
struct NodeId {
    run: RunId,
    sequence: u64,
}

impl fmt::Display for NodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.run, self.sequence)
    }
}

/// Requests cooperative cancellation of the [`CancelSignal`] it was paired with.
///
/// Every clone addresses the same signal. Dropping every handle never cancels: a process whose
/// handles are all gone simply runs to completion as if it were non-interruptible.
#[derive(Clone)]
pub struct CancelHandle {
    sender: watch::Sender<bool>,
}

impl CancelHandle {
    /// Requests cancellation. Repeated calls are idempotent.
    ///
    /// The request is cooperative: [`ProcessRun`] aborts the node's Tokio task at its next
    /// `.await`, then joins it and reports [`ProcessOutcome::TaskFailed`] whose
    /// [`JoinError::is_cancelled`](tokio::task::JoinError::is_cancelled) is `true`. A process that
    /// already returned keeps its real result. Only the node's own task is joined: work it handed
    /// to [`tokio::task::spawn_blocking`] or spawned as another task is detached by the abort, is
    /// not joined, and can outlive the `cancelled` outcome.
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }
}

/// The cancellation request a cancellable process is supervised against.
///
/// Build one with [`CancelSignal::pair`] to keep a [`CancelHandle`], or with
/// [`CancelSignal::never`] for a process that is cancellable in contract but has no requester yet.
///
/// # Example
///
/// ```
/// use std::io;
///
/// use dekopon_process::{CancelSignal, ProcessMetadata, ProcessOutcome, ProcessRun, process_fn};
///
/// # #[tokio::main]
/// # async fn main() {
/// let (handle, signal) = CancelSignal::pair();
/// let process = process_fn(ProcessMetadata::cancellable("example", signal), || async {
///     std::future::pending::<Result<(), io::Error>>().await
/// });
/// handle.cancel();
/// match ProcessRun::execute(process, |_| {}).await {
///     ProcessOutcome::TaskFailed(error) => assert!(error.is_cancelled()),
///     ProcessOutcome::Completed(_) => panic!("a parked process cannot complete"),
/// }
/// # }
/// ```
#[derive(Clone)]
pub struct CancelSignal {
    receiver: watch::Receiver<bool>,
}

impl CancelSignal {
    /// Creates a signal together with the handle that requests it.
    #[must_use]
    pub fn pair() -> (CancelHandle, Self) {
        let (sender, receiver) = watch::channel(false);
        (CancelHandle { sender }, Self { receiver })
    }

    /// Creates a signal that nobody can ever request.
    ///
    /// Its sender is dropped immediately, which the supervisor treats as "pend forever", never as
    /// a cancellation.
    #[must_use]
    pub fn never() -> Self {
        let (sender, receiver) = watch::channel(false);
        drop(sender);
        Self { receiver }
    }

    /// Resolves only once cancellation has been requested.
    async fn cancelled(&mut self) {
        loop {
            if *self.receiver.borrow_and_update() {
                return;
            }
            match self.receiver.changed().await {
                Ok(()) => {}
                Err(closed) => {
                    tracing::debug!(
                        cause = %closed,
                        "every cancel handle is gone; the process is now joined without cancellation"
                    );
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

#[derive(Clone)]
enum Interruptibility {
    NonInterruptible,
    Cancellable(CancelSignal),
}

impl Interruptibility {
    const fn label(&self) -> &'static str {
        match self {
            Self::NonInterruptible => "non-interruptible",
            Self::Cancellable(_) => "cancellable",
        }
    }
}

/// Fixed, payload-free metadata for one process operation.
///
/// Once an operation starts, [`ProcessRun`] always awaits its Tokio task. It never reports any
/// outcome, cancellation included, while operation work could still be running.
#[derive(Clone)]
pub struct ProcessMetadata {
    kind: &'static str,
    interruptibility: Interruptibility,
}

impl ProcessMetadata {
    /// Describes an operation that must be joined after it starts and cannot be interrupted.
    #[must_use]
    pub const fn non_interruptible(kind: &'static str) -> Self {
        Self {
            kind,
            interruptibility: Interruptibility::NonInterruptible,
        }
    }

    /// Describes an operation whose Tokio task is aborted, then joined, once `signal` is requested.
    #[must_use]
    pub fn cancellable(kind: &'static str, signal: CancelSignal) -> Self {
        Self {
            kind,
            interruptibility: Interruptibility::Cancellable(signal),
        }
    }
}

/// One asynchronous operation joined by [`ProcessRun`].
///
/// The operation's associated error is returned unchanged inside [`ProcessOutcome::Completed`].
/// Only failure of the Tokio task itself is classified separately.
#[async_trait]
pub trait Process: Send + 'static {
    /// The value returned by a completed operation.
    type Output: Send + 'static;
    /// The operation's own typed error.
    type Error: Error + Send + Sync + 'static;

    /// Returns fixed metadata used for payload-free tracing.
    fn metadata(&self) -> ProcessMetadata;

    /// Runs the operation to its typed result.
    async fn run(self) -> Result<Self::Output, Self::Error>;
}

/// [`Process`] implementation backed by one `FnOnce` asynchronous closure.
pub struct ProcessFn<F> {
    metadata: ProcessMetadata,
    function: F,
}

/// Adapts an asynchronous closure to [`Process`].
#[must_use]
pub fn process_fn<F, Fut, Output, OperationError>(
    metadata: ProcessMetadata,
    function: F,
) -> ProcessFn<F>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Output, OperationError>>,
{
    ProcessFn { metadata, function }
}

#[async_trait]
impl<F, Fut, Output, OperationError> Process for ProcessFn<F>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<Output, OperationError>> + Send,
    Output: Send + 'static,
    OperationError: Error + Send + Sync + 'static,
{
    type Output = Output;
    type Error = OperationError;

    fn metadata(&self) -> ProcessMetadata {
        self.metadata.clone()
    }

    async fn run(self) -> Result<Self::Output, Self::Error> {
        (self.function)().await
    }
}

/// Terminal result of one joined process operation.
#[must_use = "process outcomes must be handled"]
pub enum ProcessOutcome<Output, OperationError> {
    /// The process returned its typed operation result.
    Completed(Result<Output, OperationError>),
    /// The Tokio task was cancelled or panicked before returning an operation result.
    ///
    /// A requested cancellation arrives here with
    /// [`JoinError::is_cancelled`](tokio::task::JoinError::is_cancelled) set, and only after the
    /// supervisor has joined the aborted task.
    TaskFailed(tokio::task::JoinError),
}

/// One-run/one-node Tokio execution boundary.
///
/// The run and node identities exist only as trace fields; Tokio task IDs are not application
/// identity. `execute` transfers the process into a self-contained supervisor before its first
/// await. While the owning Tokio runtime remains alive, the supervisor owns and joins the node even
/// if the caller drops the `execute` future. Runtime shutdown is the ownership boundary.
///
/// # Example
///
/// ```
/// use std::io;
///
/// use dekopon_process::{
///     ProcessMetadata, ProcessOutcome, ProcessRun, process_fn,
/// };
///
/// # #[tokio::main]
/// # async fn main() {
///     let process = process_fn(
///         ProcessMetadata::non_interruptible("example"),
///         || async { Ok::<_, io::Error>(42_u8) },
///     );
///
///     let on_unobserved = |outcome| match outcome {
///         ProcessOutcome::Completed(Ok(_)) => eprintln!("unobserved process succeeded"),
///         ProcessOutcome::Completed(Err(error)) => eprintln!("unobserved error: {error}"),
///         ProcessOutcome::TaskFailed(error) => eprintln!("unobserved task failure: {error}"),
///     };
///     match ProcessRun::execute(process, on_unobserved).await {
///         ProcessOutcome::Completed(Ok(value)) => assert_eq!(value, 42),
///         ProcessOutcome::Completed(Err(error)) => panic!("operation failed: {error}"),
///         ProcessOutcome::TaskFailed(error) => panic!("task failed: {error}"),
///     }
/// }
/// ```
pub struct ProcessRun {
    _private: (),
}

struct OutcomeEnvelope<Outcome, Observer>
where
    Observer: FnOnce(Outcome),
{
    outcome: Option<Outcome>,
    observer: Option<Observer>,
}

impl<Outcome, Observer> OutcomeEnvelope<Outcome, Observer>
where
    Observer: FnOnce(Outcome),
{
    fn new(outcome: Outcome, observer: Observer) -> Self {
        Self {
            outcome: Some(outcome),
            observer: Some(observer),
        }
    }

    fn claim(mut self) -> Outcome {
        let outcome = self
            .outcome
            .take()
            .expect("an unclaimed envelope always contains its outcome");
        let observer = self
            .observer
            .take()
            .expect("an unclaimed envelope always contains its observer");
        drop(observer);
        outcome
    }
}

impl<Outcome, Observer> Drop for OutcomeEnvelope<Outcome, Observer>
where
    Observer: FnOnce(Outcome),
{
    fn drop(&mut self) {
        match (self.outcome.take(), self.observer.take()) {
            (Some(outcome), Some(observer)) => observer(outcome),
            (None, None) => {}
            _ => unreachable!("outcome and observer are always claimed together"),
        }
    }
}

impl ProcessRun {
    /// Runs one process in a traced Tokio task and joins it before returning.
    ///
    /// A cancellable process is supervised against its [`CancelSignal`]: once requested, the
    /// node's task is aborted and then still joined, so this never returns while the node could
    /// be running. A node that finished before the abort landed keeps its real result.
    ///
    /// If this future is dropped while its Tokio runtime remains alive, the internal supervisor
    /// continues joining the node and delivers its full outcome to `on_unobserved`. A private RAII
    /// envelope also invokes the observer if a delivered-but-unclaimed outcome is abandoned. The
    /// callback must not panic and is responsible for handling every abandoned success or failure
    /// without leaking operation payloads into telemetry.
    pub async fn execute<P, Observer>(
        process: P,
        on_unobserved: Observer,
    ) -> ProcessOutcome<P::Output, P::Error>
    where
        P: Process,
        Observer: FnOnce(ProcessOutcome<P::Output, P::Error>) + Send + 'static,
    {
        let metadata = process.metadata();
        let run_id = RunId(NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed));
        let node_id = NodeId {
            run: run_id,
            sequence: 1,
        };
        let run_span = tracing::debug_span!(
            "process.run",
            run.id = %run_id,
        );
        let run_instrument = run_span.clone().or_current();
        let node_span = tracing::debug_span!(
            parent: &run_span,
            "process.node",
            run.id = %run_id,
            node.id = %node_id,
            parent.id = "root",
            process.kind = metadata.kind,
            process.interruptibility = metadata.interruptibility.label(),
            process.outcome = tracing::field::Empty,
        );
        let node_instrument = node_span.clone().or_current();
        let outcome_span = node_span;
        let interruptibility = metadata.interruptibility;

        let (outcome_sender, outcome_receiver) = tokio::sync::oneshot::channel();
        // There is deliberately no await between constructing this supervisor and moving
        // `process` into `tokio::spawn`. Once admitted, the supervisor owns the process node and,
        // while the runtime lives, remains responsible for joining, recording, and delivering it
        // even if this outer future is dropped.
        let supervisor = tokio::spawn(
            async move {
                let mut task = tokio::spawn(process.run().instrument(node_instrument));
                let (joined, cancel_requested) = match interruptibility {
                    Interruptibility::NonInterruptible => (task.await, false),
                    Interruptibility::Cancellable(mut signal) => {
                        tokio::select! {
                            biased;
                            joined = &mut task => (joined, false),
                            () = signal.cancelled() => {
                                // Abort is cooperative: it lands at the node's next await. The
                                // join below is what makes the outcome safe to report.
                                task.abort();
                                (task.await, true)
                            }
                        }
                    }
                };
                let outcome = match joined {
                    Ok(result) => {
                        outcome_span.in_scope(|| {
                            outcome_span.record(
                                "process.outcome",
                                if result.is_ok() {
                                    "succeeded"
                                } else {
                                    "operation-error"
                                },
                            );
                        });
                        ProcessOutcome::Completed(result)
                    }
                    Err(error) => {
                        outcome_span.in_scope(|| {
                            outcome_span.record(
                                "process.outcome",
                                if error.is_panic() {
                                    "panicked"
                                } else if cancel_requested {
                                    "cancelled"
                                } else {
                                    "task-cancelled"
                                },
                            );
                        });
                        ProcessOutcome::TaskFailed(error)
                    }
                };
                let envelope = OutcomeEnvelope::new(outcome, on_unobserved);
                if let Err(envelope) = outcome_sender.send(envelope) {
                    drop(envelope);
                }
            }
            .instrument(run_instrument),
        );

        match outcome_receiver.await {
            Ok(envelope) => envelope.claim(),
            Err(receive_error) => match supervisor.await {
                Err(error) => ProcessOutcome::TaskFailed(error),
                Ok(()) => panic!(
                    "a successful process supervisor always delivers an outcome: {receive_error}"
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests;
