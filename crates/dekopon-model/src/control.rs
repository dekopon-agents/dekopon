//! Per-generation cancellation and a single total deadline.

use crate::error::{InferenceError, RequestError};
use std::{future::Future, time::Duration};
use tokio::{sync::watch, time::Instant};

/// One call's session cancellation signal and total deadline.
pub struct TurnControl {
    cancel: watch::Receiver<bool>,
    deadline: Instant,
}

impl TurnControl {
    /// Starts a fresh call deadline without changing the session's cancellation signal.
    pub fn new(cancel: watch::Receiver<bool>, timeout: Duration) -> Result<Self, InferenceError> {
        if timeout.is_zero() {
            return Err(RequestError::ZeroTimeout.into());
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RequestError::DeadlineOutOfRange)?;
        Ok(Self { cancel, deadline })
    }

    /// Reject work before scheduling a blocking credential or attachment operation.
    pub(crate) fn check(&self) -> Result<(), InferenceError> {
        if *self.cancel.borrow() {
            return Err(InferenceError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(InferenceError::DeadlineExceeded);
        }
        Ok(())
    }

    async fn cancelled(&self) {
        let mut cancel = self.cancel.clone();
        loop {
            if *cancel.borrow_and_update() {
                return;
            }
            if cancel.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    pub(crate) async fn run<T>(&self, work: impl Future<Output = T>) -> Result<T, InferenceError> {
        tokio::select! {
            biased;
            () = self.cancelled() => Err(InferenceError::Cancelled),
            () = tokio::time::sleep_until(self.deadline) => Err(InferenceError::DeadlineExceeded),
            result = work => Ok(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_timeout_is_refused() {
        assert!(matches!(
            TurnControl::new(watch::channel(false).1, Duration::ZERO),
            Err(InferenceError::InvalidRequest(RequestError::ZeroTimeout))
        ));
    }

    #[tokio::test]
    async fn a_closed_uncancelled_sender_does_not_cancel_work() {
        let control = TurnControl::new(watch::channel(false).1, Duration::from_secs(1)).unwrap();
        assert_eq!(
            control
                .run(async {
                    tokio::task::yield_now().await;
                    7
                })
                .await
                .unwrap(),
            7
        );
    }
}
