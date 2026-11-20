#![expect(dead_code, reason = "skeleton: daemon/reload-consumers")]

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio_rustls::TlsAcceptor;

use super::{Listener, TlsError, TlsFiles};

#[derive(Clone)]
pub struct TlsReload {
    files: TlsFiles,
    identity: String,
    acceptor: Arc<ArcSwap<TlsAcceptor>>,
}

impl TlsReload {
    pub const fn files(&self) -> &TlsFiles {
        &self.files
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/reload-consumers")]
    pub fn reload(&self) -> Result<(), TlsError> {
        todo!()
    }
}

impl Listener {
    #[expect(clippy::todo, reason = "skeleton: daemon/reload-consumers")]
    pub fn tls_reload(&self) -> TlsReload {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn a_reload_swaps_the_acceptor_and_accepting_stats_nothing() {}

    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn a_half_swapped_bundle_is_refused_and_the_old_acceptor_serves() {}

    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn a_retired_acceptor_drains_with_its_connections() {}
}
