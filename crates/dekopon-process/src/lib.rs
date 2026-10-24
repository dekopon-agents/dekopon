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
use tokio::sync::watch;

#[derive(Clone)]
pub struct CancelHandle {
    sender: watch::Sender<bool>,
}

impl CancelHandle {
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }
}

#[derive(Clone)]
pub struct CancelSignal {
    receiver: watch::Receiver<bool>,
}

impl CancelSignal {
    #[must_use]
    pub fn pair() -> (CancelHandle, Self) {
        let (sender, receiver) = watch::channel(false);
        (CancelHandle { sender }, Self { receiver })
    }

    #[must_use]
    pub fn never() -> Self {
        let (sender, receiver) = watch::channel(false);
        drop(sender);
        Self { receiver }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    #[must_use]
    pub fn watch(&self) -> watch::Receiver<bool> {
        self.receiver.clone()
    }

    pub async fn cancelled(&self) {
        let mut receiver = self.watch();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

#[cfg(test)]
mod tests;
