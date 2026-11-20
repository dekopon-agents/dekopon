#![expect(dead_code, reason = "skeleton: daemon/reload-consumers")]

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio_rustls::TlsAcceptor;

use super::{Listener, TlsError, TlsFiles};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BundleDigest([u8; 32]);

#[derive(Clone, Debug)]
pub struct TlsSource {
    files: TlsFiles,
    identity: String,
    applied: BundleDigest,
}

pub enum Prepared {
    Unchanged,
    Ready(PreparedTls),
}

#[must_use]
pub struct PreparedTls {
    bundle: BundleDigest,
    acceptor: TlsAcceptor,
}

#[derive(Debug, thiserror::Error)]
#[error("the proxy TLS bundle was refused")]
pub struct TlsRefusal {
    bundle: BundleDigest,
    #[source]
    error: TlsError,
}

pub struct TlsReload {
    source: TlsSource,
    acceptor: Arc<ArcSwap<TlsAcceptor>>,
    refused: Option<BundleDigest>,
}

impl TlsSource {
    pub const fn files(&self) -> &TlsFiles {
        &self.files
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/reload-consumers")]
    pub fn prepare(self) -> Result<Prepared, TlsRefusal> {
        todo!()
    }
}

impl TlsReload {
    pub const fn source(&self) -> &TlsSource {
        &self.source
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/reload-consumers"
    )]
    pub fn commit(&mut self, prepared: PreparedTls) {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/reload-consumers"
    )]
    pub fn first_refusal(&mut self, refusal: &TlsRefusal) -> bool {
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
    fn an_aborted_tls_reload_keeps_the_old_acceptor() {}

    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn a_restarted_proxy_serves_the_mounted_bundle() {}

    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn the_bound_bundle_reads_as_unchanged_and_a_refusal_reports_once() {}

    #[test]
    #[ignore = "skeleton: daemon/reload-consumers"]
    fn a_retired_acceptor_drains_with_its_connections() {}
}
