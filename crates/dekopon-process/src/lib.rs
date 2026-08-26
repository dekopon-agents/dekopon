//! Joined Tokio process lifecycle for unprivileged Dekopon frontends.
//!
//! This first slice owns exactly one boundary: run one non-interruptible asynchronous operation in
//! a traced Tokio task and join it before returning. Structured process trees, ports, cooperative
//! cancellation, deadlines, and graph scheduling are intentionally deferred until a production
//! frontend consumes them.

#![forbid(unsafe_code)]

use std::{
    error::Error,
    fmt,
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
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

/// Fixed, payload-free metadata for one non-interruptible process operation.
///
/// Once an operation starts, [`ProcessRun`] always awaits its Tokio task. It never reports
/// cancellation while operation work could still be running.
pub struct ProcessMetadata {
    kind: &'static str,
}

impl ProcessMetadata {
    /// Describes an operation that must be joined after it starts.
    #[must_use]
    pub const fn non_interruptible(kind: &'static str) -> Self {
        Self { kind }
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
        ProcessMetadata {
            kind: self.metadata.kind,
        }
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
    /// Runs one non-interruptible process in a traced Tokio task and joins it before returning.
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
            process.interruptibility = "non-interruptible",
            process.outcome = tracing::field::Empty,
        );
        let node_instrument = node_span.clone().or_current();
        let outcome_span = node_span;

        let (outcome_sender, outcome_receiver) = tokio::sync::oneshot::channel();
        // There is deliberately no await between constructing this supervisor and moving
        // `process` into `tokio::spawn`. Once admitted, the supervisor owns the process node and,
        // while the runtime lives, remains responsible for joining, recording, and delivering it
        // even if this outer future is dropped.
        let supervisor = tokio::spawn(
            async move {
                let task = tokio::spawn(process.run().instrument(node_instrument));
                let outcome = match task.await {
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
