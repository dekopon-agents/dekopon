use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use dekopon_core::{AgentId, Redacted};
use dekopon_model_proxy::{
    ANTHROPIC_ENDPOINT, CODEX_ENDPOINT, Grant, ModelProxy, OPENROUTER_ENDPOINT, ProxyModel,
    Upstream,
    tls::{Listener, TlsFiles},
};
use dekopon_model_token_governor::Metering;
use serde::Deserialize;
use thiserror::Error;

use crate::{
    config::ModelConfig,
    session::{ConfiguredModels, ModelCredentialError, model_credential},
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProxyConfig {
    pub bind: SocketAddr,
    pub tls: ProxyTlsConfig,
    pub jail_identity: String,
    #[serde(default)]
    pub guests: BTreeMap<String, GuestConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProxyTlsConfig {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    pub client_ca_file: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GuestConfig {
    pub agent: String,
    pub models: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ProxyProblem {
    #[error("proxy guest {subject:?} names agent {agent:?}, which no route serves")]
    UnknownAgent { subject: String, agent: String },
    #[error("proxy guest {subject:?} names unknown model {model:?}")]
    UnknownModel { subject: String, model: String },
    #[error(
        "proxy guest {subject:?} names model {model:?}, whose kind openaiCompatible the proxy cannot serve"
    )]
    UnservableModel { subject: String, model: String },
    #[error("proxy guest subject must not be empty")]
    EmptySubject,
    #[error("proxy jailIdentity must be a URI")]
    InvalidJailIdentity,
    #[error("proxy TLS files are unusable")]
    Tls(#[source] dekopon_model_proxy::tls::TlsError),
}

#[derive(Clone, Debug)]
pub struct ResolvedProxy {
    pub bind: SocketAddr,
    pub files: TlsFiles,
    pub jail_identity: String,
    pub guests: BTreeMap<String, (AgentId, BTreeSet<String>)>,
}

pub(crate) fn resolve(
    config: Option<ProxyConfig>,
    agents: &BTreeSet<String>,
    models: &[ModelConfig],
    resolve_path: impl Fn(PathBuf) -> PathBuf,
    problems: &mut Vec<ProxyProblem>,
) -> Option<ResolvedProxy> {
    let config = config?;
    let files = TlsFiles {
        cert: resolve_path(config.tls.cert_file),
        key: resolve_path(config.tls.key_file),
        client_ca: resolve_path(config.tls.client_ca_file),
    };
    if !config.jail_identity.contains("://") {
        problems.push(ProxyProblem::InvalidJailIdentity);
    }
    if let Err(error) = dekopon_model_proxy::tls::server_config(&files, &config.jail_identity) {
        problems.push(ProxyProblem::Tls(error));
    }
    let mut guests = BTreeMap::new();
    for (subject, guest) in config.guests {
        if subject.trim().is_empty() {
            problems.push(ProxyProblem::EmptySubject);
        }
        let agent = guest
            .agent
            .parse::<AgentId>()
            .ok()
            .filter(|_| agents.contains(&guest.agent));
        if agent.is_none() {
            problems.push(ProxyProblem::UnknownAgent {
                subject: subject.clone(),
                agent: guest.agent.clone(),
            });
        }
        for name in &guest.models {
            match models.iter().find(|model| model.name() == name) {
                None => problems.push(ProxyProblem::UnknownModel {
                    subject: subject.clone(),
                    model: name.clone(),
                }),
                Some(ModelConfig::OpenaiCompatible { .. }) => {
                    problems.push(ProxyProblem::UnservableModel {
                        subject: subject.clone(),
                        model: name.clone(),
                    });
                }
                Some(
                    ModelConfig::Openrouter { .. }
                    | ModelConfig::ChatgptSubscription { .. }
                    | ModelConfig::Anthropic { .. },
                ) => {}
            }
        }
        if let Some(agent) = agent {
            guests.insert(subject, (agent, guest.models.into_iter().collect()));
        }
    }
    Some(ResolvedProxy {
        bind: config.bind,
        files,
        jail_identity: config.jail_identity,
        guests,
    })
}

#[derive(Debug, Error)]
pub enum ProxyStartError {
    #[error(transparent)]
    Credential(#[from] ModelCredentialError),
    #[error("proxy model {model:?} credential could not be opened")]
    Codex {
        model: String,
        #[source]
        source: Box<dekopon_model::error::InferenceError>,
    },
    #[error(transparent)]
    Client(#[from] dekopon_model_proxy::ClientError),
    #[error(transparent)]
    Listen(#[from] dekopon_model_proxy::tls::ListenError),
}

/// Binds the listener and spawns it into `tasks`, whose owner aborts it at shutdown.
pub(crate) async fn start(
    proxy: &ResolvedProxy,
    models: &[ModelConfig],
    credentials: &ConfiguredModels,
    metering: &Arc<Metering>,
    tasks: &mut tokio::task::JoinSet<()>,
) -> Result<std::net::SocketAddr, ProxyStartError> {
    start_with(proxy, models, credentials, metering, tasks, |variable| {
        std::env::var_os(variable)
    })
    .await
}

pub(crate) async fn start_with(
    proxy: &ResolvedProxy,
    models: &[ModelConfig],
    credentials: &ConfiguredModels,
    metering: &Arc<Metering>,
    tasks: &mut tokio::task::JoinSet<()>,
    resolve: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<std::net::SocketAddr, ProxyStartError> {
    let granted = proxy
        .guests
        .values()
        .flat_map(|(_, models)| models)
        .collect::<BTreeSet<_>>();
    let mut upstreams = Vec::new();
    for model in models
        .iter()
        .filter(|model| granted.contains(&model.name().to_owned()))
    {
        let upstream = match model {
            ModelConfig::Anthropic {
                name, api_key_env, ..
            } => Upstream::Anthropic {
                endpoint: ANTHROPIC_ENDPOINT.to_owned(),
                api_key: Redacted::new(model_credential(name, api_key_env, resolve(api_key_env))?),
            },
            ModelConfig::Openrouter {
                name, api_key_env, ..
            } => Upstream::OpenRouter {
                endpoint: OPENROUTER_ENDPOINT.to_owned(),
                api_key: Redacted::new(model_credential(name, api_key_env, resolve(api_key_env))?),
            },
            ModelConfig::ChatgptSubscription {
                name,
                auth_file,
                timeout_ms,
                ..
            } => Upstream::Codex {
                endpoint: CODEX_ENDPOINT.to_owned(),
                credential: credentials
                    .chatgpt_credential(
                        auth_file.as_deref(),
                        std::time::Duration::from_millis(*timeout_ms),
                    )
                    .map_err(|source| ProxyStartError::Codex {
                        model: name.clone(),
                        source: Box::new(source),
                    })?,
            },
            ModelConfig::OpenaiCompatible { .. } => continue,
        };
        upstreams.push(ProxyModel {
            name: model.name().to_owned(),
            wire_model: wire_model(model).to_owned(),
            backend: model.backend(),
            reserve: model.output_reserve(),
            upstream,
        });
    }
    let guests = proxy
        .guests
        .iter()
        .map(|(subject, (agent, models))| {
            (
                subject.clone(),
                Grant {
                    agent: agent.clone(),
                    models: models.clone(),
                },
            )
        })
        .collect::<HashMap<_, _>>();
    let router = Arc::new(ModelProxy::new(upstreams, guests, Arc::clone(metering))?).router();
    let listener =
        Listener::bind(proxy.bind, proxy.files.clone(), proxy.jail_identity.clone()).await?;
    let address =
        listener
            .local_addr()
            .map_err(|source| dekopon_model_proxy::tls::ListenError::Bind {
                bind: proxy.bind,
                source,
            })?;
    tasks.spawn(listener.serve(router, std::future::pending()));
    tracing::info!(
        event = "gateway_proxy_listening",
        proxy.port = address.port()
    );
    Ok(address)
}

fn wire_model(model: &ModelConfig) -> &str {
    match model {
        ModelConfig::OpenaiCompatible { model, .. }
        | ModelConfig::Openrouter { model, .. }
        | ModelConfig::ChatgptSubscription { model, .. }
        | ModelConfig::Anthropic { model, .. } => model,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use dekopon_broker_protocol::BrokerSocketDiscovery;
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, SanType};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::config::{ConfigError, ConfigProblem, DekopondConfig};

    const JAIL: &str = "spiffe://homelab/ns/vm-runner/sa/vm-runner-jail";

    struct Pki {
        directory: tempfile::TempDir,
        server_ca: CertifiedIssuer<'static, KeyPair>,
        jail: (String, String),
    }

    fn issuer() -> CertifiedIssuer<'static, KeyPair> {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
    }

    fn leaf(
        issuer: &CertifiedIssuer<'static, KeyPair>,
        dns: &[&str],
        uri: Option<&str>,
    ) -> (String, String) {
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
        let key = KeyPair::generate().unwrap();
        (
            params.signed_by(&key, issuer).unwrap().pem(),
            key.serialize_pem(),
        )
    }

    fn pki() -> Pki {
        let directory = tempfile::TempDir::new().unwrap();
        let server_ca = issuer();
        let jail_ca = issuer();
        let (cert, key) = leaf(&server_ca, &["dekopond.test"], None);
        std::fs::write(directory.path().join("tls.crt"), cert).unwrap();
        std::fs::write(directory.path().join("tls.key"), key).unwrap();
        std::fs::write(directory.path().join("ca.crt"), jail_ca.pem()).unwrap();
        let jail = leaf(&jail_ca, &[], Some(JAIL));
        Pki {
            directory,
            server_ca,
            jail,
        }
    }

    fn document(directory: &Path, proxy: &str) -> String {
        format!(
            "apiVersion: dekopon.dev/dekopond/v1alpha1\n\
             catalogPath: dekopon.yaml\n\
             broker: {{ socketPath: /run/dekopon/broker.sock, serverUid: 501 }}\n\
             transports: [{{name: dev, kind: local, socketPath: dev.sock}}]\n\
             models:\n\
             - {{name: local, kind: openaiCompatible, endpoint: 'http://127.0.0.1:11434/v1', model: q, timeoutMs: 1000, classes: [reasoning]}}\n\
             - {{name: claude-opus, kind: anthropic, model: claude-opus-4-1, apiKeyEnv: ANTHROPIC_API_KEY}}\n\
             - {{name: glm-flash, kind: openrouter, model: z-ai/glm-4.5-air, apiKeyEnv: OPENROUTER_API_KEY, timeoutMs: 1000}}\n\
             routes:\n\
             - {{transport: dev, conversation: {{kind: [directMessage]}}, agent: gylmar}}\n\
             proxy:\n\
             \x20 bind: 127.0.0.1:0\n\
             \x20 tls: {{certFile: {dir}/tls.crt, keyFile: {dir}/tls.key, clientCaFile: {dir}/ca.crt}}\n\
             \x20 jailIdentity: {JAIL}\n\
             {proxy}",
            dir = directory.display()
        )
    }

    fn resolved(document: &str) -> Result<crate::config::ResolvedConfig, ConfigError> {
        let config = serde_yaml::from_str::<DekopondConfig>(document).expect("the fixture decodes");
        crate::config::resolve(
            config,
            PathBuf::from("/tmp/dekopond.yaml"),
            &BrokerSocketDiscovery::new(None, None, Some(PathBuf::from("/run/user/501")), None),
            501,
        )
    }

    #[test]
    fn a_proxy_block_resolves_its_guests() {
        let pki = pki();
        let config = resolved(&document(
            pki.directory.path(),
            "  guests:\n    'dekopon:gylmar-vm': {agent: gylmar, models: [claude-opus, glm-flash]}\n",
        ))
        .unwrap();
        let proxy = config.proxy.unwrap();
        let (agent, models) = &proxy.guests["dekopon:gylmar-vm"];
        assert_eq!(agent.as_str(), "gylmar");
        assert_eq!(models.len(), 2);
    }

    #[test]
    fn every_proxy_problem_is_reported_at_once() {
        let pki = pki();
        std::fs::remove_file(pki.directory.path().join("ca.crt")).unwrap();
        let document = document(
            pki.directory.path(),
            "  guests:\n    'dekopon:ghost-vm': {agent: ghost, models: [nope, local]}\n",
        )
        .replace("agent: gylmar}", "agent: gylmar, model: claude-opus}");
        let Err(ConfigError::Invalid { problems, .. }) = resolved(&document) else {
            panic!("the proxy block is refused");
        };
        let rendered = problems.iter().map(ToString::to_string).collect::<Vec<_>>();
        for expected in [
            "names agent \"ghost\"",
            "unknown model \"nope\"",
            "model \"local\", whose kind openaiCompatible",
            "TLS files are unusable",
            "served only through the guest model proxy",
        ] {
            assert!(
                rendered.iter().any(|problem| problem.contains(expected)),
                "{expected}: {rendered:?}"
            );
        }
        assert!(
            problems
                .iter()
                .any(|problem| matches!(problem, ConfigProblem::ProxyOnlyRouteModel { .. }))
        );
    }

    #[test]
    fn a_proxy_without_tls_does_not_decode() {
        assert!(
            serde_yaml::from_str::<ProxyConfig>("bind: 0.0.0.0:9090\njailIdentity: spiffe://x\n")
                .is_err()
        );
    }

    async fn ask(address: SocketAddr, pki: &Pki, subject: &str, body: &str) -> String {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(pki.server_ca.der().clone()).unwrap();
        let (cert, key) = &pki.jail;
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_client_auth_cert(
            rustls::pki_types::pem::PemObject::pem_slice_iter(cert.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            rustls::pki_types::pem::PemObject::from_pem_slice(key.as_bytes()).unwrap(),
        )
        .unwrap();
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect("dekopond.test".try_into().unwrap(), stream)
            .await
            .unwrap();
        tls.write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: dekopond.test\r\nx-dekopon-vm-subject: {subject}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut response = String::new();
        tls.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn the_started_listener_refuses_unknown_subjects_and_ungranted_models() {
        let pki = pki();
        let config = resolved(&document(
            pki.directory.path(),
            "  guests:\n    'dekopon:gylmar-vm': {agent: gylmar, models: [claude-opus]}\n",
        ))
        .unwrap();
        let metering = Arc::new(Metering::new(Vec::new(), Metering::system_clock()));
        let mut tasks = tokio::task::JoinSet::new();
        let address = start_with(
            config.proxy.as_ref().unwrap(),
            &config.models,
            &ConfiguredModels::default(),
            &metering,
            &mut tasks,
            |_| Some("synthetic-key".into()),
        )
        .await
        .unwrap();
        let stranger = ask(
            address,
            &pki,
            "dekopon:stranger-vm",
            r#"{"model":"claude-opus"}"#,
        )
        .await;
        assert!(stranger.starts_with("HTTP/1.1 403"), "{stranger}");
        assert!(stranger.contains("permission_error"), "{stranger}");
        let ungranted = ask(
            address,
            &pki,
            "dekopon:gylmar-vm",
            r#"{"model":"glm-flash"}"#,
        )
        .await;
        assert!(ungranted.starts_with("HTTP/1.1 403"), "{ungranted}");
        assert!(ungranted.contains("permission_error"), "{ungranted}");
        assert!(
            ungranted.contains(
                "dekopon sandbox: this agent may only call its configured models: `claude-opus`."
            ),
            "{ungranted}"
        );
        tasks.shutdown().await;
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }
}
