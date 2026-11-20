#![expect(
    dead_code,
    reason = "skeleton: daemon/notice-channel wires the notice into boot and serving"
)]

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use dekopon_agent::wake::WakeId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    transport::{ChatDriver, ReplyTarget},
    wake::TickEnd,
};

#[derive(Clone, Debug)]
pub(crate) struct Destination {
    pub(crate) transport: String,
    pub(crate) target: ReplyTarget,
}

#[derive(Clone, Debug)]
pub(crate) struct NoticeChannel {
    pub(crate) destination: Destination,
    pub(crate) stability: Duration,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FatalLabel {
    Store,
    Reload,
    Transports,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(crate) enum StandinState {}

#[derive(Debug, Error)]
pub(crate) enum MarkerError {
    #[error("could not read or write the exit marker")]
    Io { kind: io::ErrorKind },
    #[error("the exit marker did not parse; remove it to reset the notice streak")]
    Corrupt,
}

impl From<io::Error> for MarkerError {
    fn from(error: io::Error) -> Self {
        Self::Io { kind: error.kind() }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ExitMarker {
    pub(crate) streak: StandinState,
    pub(crate) first_exit_ms: u64,
    pub(crate) notice: MarkerNotice,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum MarkerNotice {
    Owed,
    Posted,
}

pub(crate) struct MarkerFile {
    path: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum NoticeLine {
    Fatal {
        cause: FatalLabel,
    },
    Recovered {
        restarts: u32,
        causes: Vec<FatalLabel>,
    },
    WakePanicked {
        wake: WakeId,
    },
    WakeRecovered {
        wake: WakeId,
    },
}

#[derive(Default)]
pub(crate) struct WakePanics {
    open: HashMap<WakeId, StandinState>,
}

impl MarkerFile {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/notice-channel"
    )]
    pub(crate) fn in_dir(dir: &Path) -> Self {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/notice-channel"
    )]
    pub(crate) fn record_exit(&self, cause: FatalLabel) -> Result<(), MarkerError> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/notice-channel")]
    pub(crate) fn claim_notice(&self) -> Result<Option<NoticeLine>, MarkerError> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/notice-channel")]
    pub(crate) fn close_after_stability(&self) -> Result<Option<NoticeLine>, MarkerError> {
        todo!()
    }
}

impl WakePanics {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/notice-channel"
    )]
    pub(crate) fn observe(&mut self, end: &TickEnd) -> Option<NoticeLine> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/notice-channel"
    )]
    pub(crate) fn retire(&mut self, wake: WakeId) {
        todo!()
    }
}

impl NoticeChannel {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/notice-channel"
    )]
    pub(crate) async fn post(&self, driver: &dyn ChatDriver, line: NoticeLine) {
        todo!()
    }
}
