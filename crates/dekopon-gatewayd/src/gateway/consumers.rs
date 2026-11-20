#![expect(dead_code, reason = "skeleton: daemon/reload-consumers")]

use std::sync::Arc;

use super::CurrentGeneration;
use crate::follow::FollowTable;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Category {
    Models,
    ExtraCaBundles,
    Routes,
    StopWords,
    Catalog,
    Probe,
    Indicators,
    Transports,
    Sessions,
    Broker,
    ShutdownGrace,
    Telemetry,
    Metering,
    Proxy,
    Reload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Disposition {
    Swap,
    Restart(Resident),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Resident {
    TransportConnections,
    SessionPool,
    BrokerClient,
    ServeLoop,
    TelemetryExporter,
    MeteringStore,
    ProxyListener,
    Watcher,
}

impl Category {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/reload-consumers"
    )]
    pub(crate) fn from_key(key: &str) -> Option<Self> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/reload-consumers")]
    pub(crate) const fn disposition(self) -> Disposition {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/reload-consumers")]
    pub(crate) const fn label(self) -> &'static str {
        todo!()
    }
}

impl CurrentGeneration {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/reload-consumers"
    )]
    pub(crate) fn swap_follow(&self, table: Option<Arc<FollowTable>>) {
        todo!()
    }
}
