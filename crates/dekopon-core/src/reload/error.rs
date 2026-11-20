use std::{io, path::PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReloadError {
    #[error("could not watch {path}")]
    Register { path: PathBuf, kind: io::ErrorKind },
    #[error("{path} is delivered once and never changes, so it cannot be watched")]
    Unwatchable { path: PathBuf },
    #[error("the file watcher backend failed")]
    Backend { kind: io::ErrorKind },
    #[error("the file watcher's event queue closed")]
    QueueClosed,
    #[error("could not install the SIGHUP handler")]
    Hangup { kind: io::ErrorKind },
    #[error("a valid reload did not settle within {bound_ms} ms")]
    ActivationTimedOut { bound_ms: u64 },
}

impl ReloadError {
    #[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
    pub const fn category(&self) -> &'static str {
        todo!()
    }
}

#[derive(Debug, Error)]
pub enum Stop<E> {
    #[error(transparent)]
    Mechanism(ReloadError),
    #[error(transparent)]
    Consumer(E),
}
