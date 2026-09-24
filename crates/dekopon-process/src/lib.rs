//! Joined Tokio process lifecycle for unprivileged Dekopon frontends.
//!
//! This slice owns exactly one boundary: run one asynchronous operation in a traced Tokio task and
//! join it before returning. A process is either non-interruptible or cancellable. Cancellation is
//! cooperative and minimal: a [`CancelHandle`] asks, [`ProcessRun::execute`] aborts the node's Tokio
//! task at its next `.await`, and still joins that task before it reports anything. It joins the
//! node's own Tokio task and nothing else: work the node handed to
//! [`tokio::task::spawn_blocking`] or spawned as another task is detached by the abort, is not
//! joined, and can outlive a `cancelled` outcome. A node that must not leave such work behind
//! must stay [`ProcessMetadata::non_interruptible`]. Structured process trees, ports, deadlines,
//! and graph scheduling remain deferred until a production frontend consumes them.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::{
    error::Error,
    fmt,
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use tokio::{sync::watch, task::JoinSet};
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
/// match ProcessRun::execute(process).await {
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
    /// Its sender is dropped immediately, which `execute` treats as "pend forever", never as
    /// a cancellation.
    #[must_use]
    pub fn never() -> Self {
        let (sender, receiver) = watch::channel(false);
        drop(sender);
        Self { receiver }
    }

    /// Reports whether cancellation has already been requested.
    ///
    /// The point-in-time read for a caller that must decide now rather than await: a synchronous
    /// boundary deciding whether to start work at all, where `execute`'s own await on this
    /// signal has nothing yet to abort. Once requested it stays `true`, and a
    /// [`CancelSignal::never`] signal is always `false`. A signal fired the instant after this
    /// returns `false` is the ordinary race any cooperative check has: work started on that answer
    /// is supervised, not lost.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Clones the cancellation watch for consumers outside the process.
    #[must_use]
    pub fn watch(&self) -> watch::Receiver<bool> {
        self.receiver.clone()
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
    /// node has been joined the aborted task.
    TaskFailed(tokio::task::JoinError),
}

/// One-run/one-node Tokio execution boundary.
///
/// The run and node identities exist only as trace fields; Tokio task IDs are not application
/// identity. The node runs in a one-task [`JoinSet`] owned by the `execute`
/// future, so dropping that future aborts the node; the one caller drives it to completion with
/// `block_on`, which never drops it early.
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
///     match ProcessRun::execute(process).await {
///         ProcessOutcome::Completed(Ok(value)) => assert_eq!(value, 42),
///         ProcessOutcome::Completed(Err(error)) => panic!("operation failed: {error}"),
///         ProcessOutcome::TaskFailed(error) => panic!("task failed: {error}"),
///     }
/// }
/// ```
pub struct ProcessRun {
    _private: (),
}

impl ProcessRun {
    /// Runs one process in a traced Tokio task and joins it before returning.
    ///
    /// A cancellable process is supervised against its [`CancelSignal`]: once requested, the
    /// node's task is aborted and then still joined, so this never returns while the node could
    /// be running. A node that finished before the abort landed keeps its real result.
    ///
    pub async fn execute<P>(process: P) -> ProcessOutcome<P::Output, P::Error>
    where
        P: Process,
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

        async move {
            // A task rather than an inline future so a panicking node reports a `JoinError`
            // instead of unwinding through the caller.
            let mut node = JoinSet::new();
            node.spawn(process.run().instrument(node_instrument));
            let (joined, cancel_requested) = match metadata.interruptibility {
                Interruptibility::NonInterruptible => (join_one(&mut node).await, false),
                Interruptibility::Cancellable(mut signal) => {
                    tokio::select! {
                        biased;
                        joined = join_one(&mut node) => (joined, false),
                        () = signal.cancelled() => {
                            // Abort is cooperative: it lands at the node's next await. The join
                            // below is what makes the outcome safe to report.
                            node.abort_all();
                            (join_one(&mut node).await, true)
                        }
                    }
                }
            };
            let label = match &joined {
                Ok(Ok(_)) => "succeeded",
                Ok(Err(_)) => "operation-error",
                Err(error) if error.is_panic() => "panicked",
                Err(_) if cancel_requested => "cancelled",
                Err(_) => "task-cancelled",
            };
            node_span.in_scope(|| node_span.record("process.outcome", label));
            match joined {
                Ok(result) => ProcessOutcome::Completed(result),
                Err(error) => ProcessOutcome::TaskFailed(error),
            }
        }
        .instrument(run_span.or_current())
        .await
    }
}

/// Joins the one node a [`ProcessRun`] spawned.
async fn join_one<T: 'static>(node: &mut JoinSet<T>) -> Result<T, tokio::task::JoinError> {
    node.join_next()
        .await
        .expect("the node set holds its node until it is joined")
}

#[cfg(test)]
mod tests;
