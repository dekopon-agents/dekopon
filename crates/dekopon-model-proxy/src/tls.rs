use std::{
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use axum::Router;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use parking_lot::Mutex;
use rustls::{
    DistinguishedName, RootCertStore, ServerConfig, SignatureScheme,
    client::danger::HandshakeSignatureValid,
    pki_types::{CertificateDer, PrivateKeyDer, UnixTime, pem::PemObject as _},
    server::{
        WebPkiClientVerifier,
        danger::{ClientCertVerified, ClientCertVerifier},
    },
};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet};
use tokio_rustls::TlsAcceptor;

/// Connections served at once; one more waits in the kernel backlog until one closes.
const MAX_CONNECTIONS: usize = 256;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub client_ca: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("could not read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    #[error("{path} holds no certificate")]
    NoCertificate { path: PathBuf },
    #[error("client CA {path} is not a usable trust anchor")]
    ClientCa {
        path: PathBuf,
        #[source]
        source: rustls::Error,
    },
    #[error("client certificate verifier could not be built")]
    Verifier(#[source] rustls::server::VerifierBuilderError),
    #[error("server certificate and key do not form a TLS configuration")]
    Config(#[source] rustls::Error),
}

/// Requires a client certificate that chains to the configured CA and carries `identity` as a URI
/// SAN; any other certificate fails the handshake.
#[derive(Debug)]
struct JailVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    identity: String,
}

impl ClientCertVerifier for JailVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;
        if uri_names(end_entity).contains(&self.identity) {
            Ok(verified)
        } else {
            tracing::warn!(target: "model", event = "proxy.client_refused", reason = "identity");
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn uri_names(certificate: &CertificateDer<'_>) -> Vec<String> {
    let Ok((_, parsed)) = x509_parser::parse_x509_certificate(certificate) else {
        return Vec::new();
    };
    let Ok(Some(names)) = parsed.subject_alternative_name() else {
        return Vec::new();
    };
    names
        .value
        .general_names
        .iter()
        .filter_map(|name| {
            if let x509_parser::extensions::GeneralName::URI(uri) = name {
                Some((*uri).to_owned())
            } else {
                None
            }
        })
        .collect()
}

pub fn server_config(files: &TlsFiles, identity: &str) -> Result<Arc<ServerConfig>, TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    for certificate in certificates(&files.client_ca)? {
        roots
            .add(certificate)
            .map_err(|source| TlsError::ClientCa {
                path: files.client_ca.clone(),
                source,
            })?;
    }
    let inner = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
        .build()
        .map_err(TlsError::Verifier)?;
    let key = PrivateKeyDer::from_pem_file(&files.key).map_err(|source| TlsError::Read {
        path: files.key.clone(),
        source,
    })?;
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(TlsError::Config)?
        .with_client_cert_verifier(Arc::new(JailVerifier {
            inner,
            identity: identity.to_owned(),
        }))
        .with_single_cert(certificates(&files.cert)?, key)
        .map_err(TlsError::Config)?;
    Ok(Arc::new(config))
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certificates = CertificateDer::pem_file_iter(path)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|source| TlsError::Read {
            path: path.to_owned(),
            source,
        })?;
    if certificates.is_empty() {
        return Err(TlsError::NoCertificate {
            path: path.to_owned(),
        });
    }
    Ok(certificates)
}

type Stamp = [Option<SystemTime>; 3];

fn stamp(files: &TlsFiles) -> Stamp {
    [&files.cert, &files.key, &files.client_ca].map(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    })
}

/// Rebuilds the TLS configuration when cert-manager rotates the mounted files, checked by mtime
/// on each accept, so a renewed certificate needs no restart.
struct Reloading {
    files: TlsFiles,
    identity: String,
    current: Mutex<(Stamp, TlsAcceptor)>,
}

impl Reloading {
    fn acceptor(&self) -> TlsAcceptor {
        let now = stamp(&self.files);
        let mut current = self.current.lock();
        if current.0 != now {
            match server_config(&self.files, &self.identity) {
                Ok(config) => *current = (now, TlsAcceptor::from(config)),
                Err(error) => {
                    tracing::warn!(target: "model", event = "proxy.tls_reload_failed", error = %error);
                }
            }
        }
        current.1.clone()
    }
}

pub struct Listener {
    tcp: TcpListener,
    tls: Reloading,
}

#[derive(Debug, thiserror::Error)]
pub enum ListenError {
    #[error(transparent)]
    Tls(#[from] TlsError),
    #[error("could not bind the model proxy to {bind}")]
    Bind {
        bind: SocketAddr,
        #[source]
        source: std::io::Error,
    },
}

impl Listener {
    pub async fn bind(
        bind: SocketAddr,
        files: TlsFiles,
        identity: String,
    ) -> Result<Self, ListenError> {
        let config = server_config(&files, &identity)?;
        let tcp = TcpListener::bind(bind)
            .await
            .map_err(|source| ListenError::Bind { bind, source })?;
        Ok(Self {
            tcp,
            tls: Reloading {
                current: Mutex::new((stamp(&files), TlsAcceptor::from(config))),
                files,
                identity,
            },
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }

    /// Serves until `shutdown` resolves, then aborts every connection; a stream cut there settles
    /// its admission as cancelled.
    pub async fn serve(self, router: Router, shutdown: impl Future<Output = ()>) {
        let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            let acquired = tokio::select! {
                () = &mut shutdown => break,
                permit = Arc::clone(&permits).acquire_owned() => permit,
            };
            let Ok(permit) = acquired else {
                break;
            };
            let accepted = tokio::select! {
                () = &mut shutdown => break,
                accepted = self.tcp.accept() => accepted,
            };
            while connections.try_join_next().is_some() {}
            let (stream, _) = match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(target: "model", event = "proxy.accept_failed", error = %error);
                    continue;
                }
            };
            let acceptor = self.tls.acceptor();
            let service = TowerToHyperService::new(router.clone());
            connections.spawn(async move {
                let _permit = permit;
                let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                    Ok(Ok(tls)) => tls,
                    Ok(Err(error)) => {
                        tracing::info!(target: "model", event = "proxy.handshake_failed", error = %error);
                        return;
                    }
                    Err(_elapsed) => {
                        tracing::info!(target: "model", event = "proxy.handshake_failed", error = "timeout");
                        return;
                    }
                };
                if let Err(error) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), service)
                    .await
                {
                    tracing::debug!(target: "model", error = %error, "proxy connection ended");
                }
            });
        }
        connections.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, SanType, date_time_ymd,
    };
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    const JAIL: &str = "spiffe://homelab/ns/vm-runner/sa/vm-runner-jail";

    struct Authority {
        issuer: CertifiedIssuer<'static, KeyPair>,
    }

    impl Authority {
        fn new(name: &str) -> Self {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, name);
            Self {
                issuer: CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap(),
            }
        }

        fn leaf(&self, dns: &[&str], uri: Option<&str>, expired: bool) -> (String, String) {
            let mut params = CertificateParams::new(
                dns.iter()
                    .map(|name| (*name).to_owned())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            if let Some(uri) = uri {
                params
                    .subject_alt_names
                    .push(SanType::URI(uri.try_into().unwrap()));
            }
            if expired {
                params.not_before = date_time_ymd(2020, 1, 1);
                params.not_after = date_time_ymd(2021, 1, 1);
            }
            let key = KeyPair::generate().unwrap();
            let cert = params.signed_by(&key, &self.issuer).unwrap();
            (cert.pem(), key.serialize_pem())
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        files: TlsFiles,
        server_ca: Authority,
        jail_ca: Authority,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::TempDir::new().unwrap();
        let files = TlsFiles {
            cert: directory.path().join("tls.crt"),
            key: directory.path().join("tls.key"),
            client_ca: directory.path().join("ca.crt"),
        };
        let server_ca = Authority::new("server ca");
        let jail_ca = Authority::new("homelab ca");
        let (cert, key) = server_ca.leaf(&["dekopond.test"], None, false);
        write(&files.cert, &cert);
        write(&files.key, &key);
        write(&files.client_ca, &jail_ca.issuer.pem());
        Fixture {
            _directory: directory,
            files,
            server_ca,
            jail_ca,
        }
    }

    async fn serve(fixture: &Fixture) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let listener = Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            fixture.files.clone(),
            JAIL.to_owned(),
        )
        .await
        .unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route("/", axum::routing::get(|| async { "served" }));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(listener.serve(router, async {
            let _stopped = stopped.await;
        }));
        (address, stop)
    }

    async fn request(
        address: SocketAddr,
        server_ca: &Authority,
        client: Option<(String, String)>,
    ) -> Result<String, std::io::Error> {
        let mut roots = RootCertStore::empty();
        roots.add(server_ca.issuer.der().clone()).unwrap();
        let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots);
        let config = match client {
            Some((cert, key)) => builder
                .with_client_auth_cert(
                    CertificateDer::pem_slice_iter(cert.as_bytes())
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                    PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap(),
                )
                .unwrap(),
            None => builder.with_no_client_auth(),
        };
        let stream = tokio::net::TcpStream::connect(address).await?;
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect("dekopond.test".try_into().unwrap(), stream)
            .await?;
        tls.write_all(b"GET / HTTP/1.1\r\nhost: dekopond.test\r\nconnection: close\r\n\r\n")
            .await?;
        let mut response = String::new();
        tls.read_to_string(&mut response).await?;
        Ok(response)
    }

    #[tokio::test]
    async fn the_jail_certificate_is_served() {
        let fixture = fixture();
        let (address, _stop) = serve(&fixture).await;
        let jail = fixture.jail_ca.leaf(&[], Some(JAIL), false);
        let response = request(address, &fixture.server_ca, Some(jail))
            .await
            .unwrap();
        assert!(response.ends_with("served"), "{response}");
    }

    #[tokio::test]
    async fn every_other_client_fails_the_handshake() {
        let fixture = fixture();
        let (address, _stop) = serve(&fixture).await;
        let stranger = Authority::new("stranger ca");
        let refused = [
            ("wrong CA", Some(stranger.leaf(&[], Some(JAIL), false))),
            (
                "wrong URI SAN",
                Some(
                    fixture
                        .jail_ca
                        .leaf(&[], Some("spiffe://homelab/ns/other/sa/other"), false),
                ),
            ),
            (
                "no URI SAN",
                Some(fixture.jail_ca.leaf(&["jail.test"], None, false)),
            ),
            ("expired", Some(fixture.jail_ca.leaf(&[], Some(JAIL), true))),
            ("no client certificate", None),
        ];
        for (case, client) in refused {
            let outcome = request(address, &fixture.server_ca, client).await;
            assert!(
                outcome.as_ref().is_err() || outcome.as_ref().is_ok_and(String::is_empty),
                "{case}: {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn rotated_files_are_picked_up_without_a_restart() {
        let fixture = fixture();
        let (address, _stop) = serve(&fixture).await;
        let renewed_server_ca = Authority::new("renewed server ca");
        let (cert, key) = renewed_server_ca.leaf(&["dekopond.test"], None, false);
        tokio::time::sleep(Duration::from_millis(20)).await;
        write(&fixture.files.cert, &cert);
        write(&fixture.files.key, &key);
        let jail = fixture.jail_ca.leaf(&[], Some(JAIL), false);
        let response = request(address, &renewed_server_ca, Some(jail.clone()))
            .await
            .unwrap();
        assert!(response.ends_with("served"), "{response}");
        assert!(
            request(address, &fixture.server_ca, Some(jail))
                .await
                .is_err()
        );
    }

    #[test]
    fn unreadable_files_are_reported_by_path() {
        let fixture = fixture();
        let missing = TlsFiles {
            client_ca: fixture.files.client_ca.with_file_name("absent.crt"),
            ..fixture.files.clone()
        };
        assert!(matches!(
            server_config(&missing, JAIL),
            Err(TlsError::Read { path, .. }) if path == missing.client_ca
        ));
    }
}
