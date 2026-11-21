use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::task::{AbortHandle, JoinError, JoinHandle};

use crate::TaskOutcome;

#[derive(Debug, thiserror::Error)]
pub enum OffloadError {
    #[error(transparent)]
    Panicked(JoinError),
    #[error(transparent)]
    Cancelled(JoinError),
}

impl From<OffloadError> for JoinError {
    fn from(error: OffloadError) -> Self {
        match error {
            OffloadError::Panicked(error) | OffloadError::Cancelled(error) => error,
        }
    }
}

impl From<OffloadError> for TaskOutcome {
    fn from(error: OffloadError) -> Self {
        match error {
            OffloadError::Panicked(_) => Self::Panicked,
            OffloadError::Cancelled(_) => Self::Cancelled,
        }
    }
}

#[must_use = "the caller owns and joins the blocking work"]
pub struct Offload<T>(JoinHandle<T>);

impl<T> Offload<T> {
    pub fn abort_handle(&self) -> AbortHandle {
        self.0.abort_handle()
    }
}

impl<T> Future for Offload<T> {
    type Output = Result<T, OffloadError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map(|result| {
            result.map_err(|error| {
                if error.is_panic() {
                    OffloadError::Panicked(error)
                } else {
                    OffloadError::Cancelled(error)
                }
            })
        })
    }
}

#[track_caller]
pub fn offload<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Offload<T> {
    #[expect(
        clippy::disallowed_methods,
        reason = "owner: the caller retains and joins the returned future; bound: the caller admits work and keeps its permit in the closure when needed"
    )]
    let task = tokio::task::spawn_blocking(work);
    Offload(task)
}

#[cfg(feature = "offload-tracing")]
pub struct OffloadTrace {
    dispatch: tracing::Dispatch,
    span: tracing::Span,
}

#[cfg(feature = "offload-tracing")]
impl OffloadTrace {
    pub fn capture(span: Option<tracing::Span>) -> Self {
        Self {
            dispatch: tracing::dispatcher::get_default(Clone::clone),
            span: span.unwrap_or_else(tracing::Span::none),
        }
    }

    pub fn in_scope<T>(self, work: impl FnOnce() -> T) -> T {
        tracing::dispatcher::with_default(&self.dispatch, || self.span.in_scope(work))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_the_closure_value() {
        assert_eq!(offload(|| 42).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn classifies_a_panicking_closure() {
        let result = offload(|| panic!("offloaded work panicked")).await;
        assert!(matches!(result, Err(OffloadError::Panicked(_))));
    }

    #[test]
    fn classifies_a_cancelled_queued_closure() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (started, ready) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let first = offload(move || {
                started.send(()).unwrap();
                released.blocking_recv().unwrap();
            });
            ready.await.unwrap();

            let queued = offload(|| 42);
            queued.abort_handle().abort();
            release.send(()).unwrap();
            first.await.unwrap();
            assert!(matches!(queued.await, Err(OffloadError::Cancelled(_))));
        });
    }

    #[cfg(feature = "offload-tracing")]
    #[tokio::test]
    async fn sets_the_captured_dispatcher_and_span_on_the_blocking_thread() {
        let (job, span_id) =
            tracing::subscriber::with_default(tracing_subscriber::registry(), || {
                let span = tracing::info_span!("offload");
                let span_id = span.id().unwrap();
                let trace = OffloadTrace::capture(Some(span));
                let job = offload(move || {
                    trace.in_scope(|| {
                        let captured = tracing::dispatcher::get_default(|dispatch| {
                            dispatch.is::<tracing_subscriber::Registry>()
                        });
                        (captured, tracing::Span::current().id())
                    })
                });
                (job, span_id)
            });
        assert_eq!(job.await.unwrap(), (true, Some(span_id)));
    }
}
