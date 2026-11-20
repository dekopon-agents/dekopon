#![expect(
    dead_code,
    reason = "skeleton: daemon/notice-channel wires the notice into boot and serving"
)]

use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, SystemTime},
};

use dekopon_agent::wake::WakeId;
use serde::{Deserialize, Serialize};

use crate::{
    store::StoreError,
    supervise::{FatalCause, FatalLabel},
    transport::ChatDriver,
};

#[derive(Clone, Debug)]
pub(crate) struct NoticeChannel {
    pub(crate) transport: String,
    pub(crate) channel: String,
    pub(crate) stability: Duration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ExitMarker {
    pub(crate) cause: FatalLabel,
    pub(crate) first_exit_ms: u64,
    pub(crate) restarts: u32,
    pub(crate) causes: Vec<FatalLabel>,
    pub(crate) notified: bool,
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
    open: HashMap<WakeId, u32>,
}

impl MarkerFile {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/notice-channel"
    )]
    pub(crate) fn in_journal(journal: PathBuf) -> Self {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/notice-channel"
    )]
    pub(crate) fn record_exit(&self, cause: &FatalCause, at: SystemTime) -> Result<(), StoreError> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/notice-channel")]
    pub(crate) fn claim_notice(&self) -> Result<Option<NoticeLine>, StoreError> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/notice-channel")]
    pub(crate) fn close_after_stability(&self) -> Result<Option<NoticeLine>, StoreError> {
        todo!()
    }
}

impl WakePanics {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/notice-channel"
    )]
    pub(crate) fn panicked(&mut self, wake: WakeId) -> Option<NoticeLine> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/notice-channel"
    )]
    pub(crate) fn succeeded(&mut self, wake: WakeId) -> Option<NoticeLine> {
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
