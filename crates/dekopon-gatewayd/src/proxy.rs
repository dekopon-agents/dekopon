use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use dekopon_core::{AgentId, Redacted};
use dekopon_model_proxy::{
    ANTHROPIC_ENDPOINT, CODEX_ENDPOINT, Grant, Granted, ModelProxy, OPENROUTER_ENDPOINT,
    ProxyModel, Upstream,
    tls::{Listener, TlsFiles},
};
use dekopon_model_token_governor::Metering;
use serde::Deserialize;
use thiserror::Error;

use crate::{
    config::ModelConfig,
    routes::RoutingTable,
    session::{ConfiguredModels, ModelCredentialError, model_credential},
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProxyConfig {
    pub bind: SocketAddr,
    pub tls: ProxyTlsConfig,
    pub jail_identity: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    #[serde(default)]
    pub guests: BTreeMap<String, GuestConfig>,
}

const DEFAULT_MAX_CONNECTIONS: usize = 16;

const fn default_max_connections() -> usize {
    DEFAULT_MAX_CONNECTIONS
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
    #[serde(default)]
    pub models: Option<Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Guest {
    Listed(BTreeSet<String>),
    Inherit,
}

pub(crate) type Grants = BTreeMap<String, (AgentId, Granted)>;

#[derive(Debug, Error)]
pub enum ProxyProblem {
    #[error("proxy guest {subject:?} names an agent which no route serves")]
    UnknownAgent { subject: String },
    #[error("proxy guest {subject:?} models[{position}] names an unknown model")]
    UnknownModel { subject: String, position: usize },
    #[error(
        "proxy guest {subject:?} names model {model:?}, whose kind openaiCompatible the proxy cannot serve"
    )]
    UnservableModel { subject: String, model: String },
    #[error(
        "proxy guest {subject:?} inherits the model of agent {agent:?}, whose routes resolve to several models: {models:?}"
    )]
    AgentModels {
        subject: String,
        agent: String,
        models: Vec<String>,
    },
    #[error("proxy guest subject must not be empty")]
    EmptySubject,
    #[error("proxy jailIdentity must be a URI")]
    InvalidJailIdentity,
    #[error("proxy maxConnections must be at least 1")]
    NoConnections,
    #[error("proxy TLS files are unusable")]
    Tls(#[source] dekopon_model_proxy::tls::TlsError),
}

#[derive(Clone, Debug)]
pub struct ResolvedProxy {
    pub bind: SocketAddr,
    pub files: TlsFiles,
    pub jail_identity: String,
    pub max_connections: usize,
    pub guests: BTreeMap<String, (AgentId, Guest)>,
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
    if config.max_connections == 0 {
        problems.push(ProxyProblem::NoConnections);
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
            });
        }
        for (position, name) in guest.models.iter().flatten().enumerate() {
            match models.iter().find(|model| model.name() == name) {
                None => problems.push(ProxyProblem::UnknownModel {
                    subject: subject.clone(),
                    position,
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
            let granted = match guest.models {
                Some(models) => Guest::Listed(models.into_iter().collect()),
                None => Guest::Inherit,
            };
            guests.insert(subject, (agent, granted));
        }
    }
    Some(ResolvedProxy {
        bind: config.bind,
        files,
        jail_identity: config.jail_identity,
        max_connections: config.max_connections,
        guests,
    })
}

pub(crate) fn listed_guests(proxy: Option<&ResolvedProxy>) -> impl Iterator<Item = &str> {
    proxy
        .map(|proxy| &proxy.guests)
        .into_iter()
        .flatten()
        .filter_map(|(subject, (_, guest))| match guest {
            Guest::Listed(_) => Some(subject.as_str()),
            Guest::Inherit => None,
        })
}

pub(crate) fn grants(
    proxy: Option<&ResolvedProxy>,
    routes: &RoutingTable,
) -> Result<Grants, Vec<ProxyProblem>> {
    let mut grants = Grants::new();
    let mut problems = Vec::new();
    for (subject, (agent, guest)) in proxy.map(|proxy| &proxy.guests).into_iter().flatten() {
        let granted = match guest {
            Guest::Listed(models) => Granted::Listed(models.clone()),
            Guest::Inherit => {
                let mut served = BTreeMap::new();
                for route in routes.routes().iter().filter(|route| &route.agent == agent) {
                    served.insert(route.model.name(), route.model.as_ref());
                }
                let mut served = served.into_iter();
                match (served.next(), served.next()) {
                    (None, _) => {
                        problems.push(ProxyProblem::UnknownAgent {
                            subject: subject.clone(),
                        });
                        continue;
                    }
                    (Some((name, ModelConfig::OpenaiCompatible { .. })), None) => {
                        problems.push(ProxyProblem::UnservableModel {
                            subject: subject.clone(),
                            model: name.to_owned(),
                        });
                        continue;
                    }
                    (
                        Some((
                            name,
                            ModelConfig::Openrouter { .. }
                            | ModelConfig::ChatgptSubscription { .. }
                            | ModelConfig::Anthropic { .. },
                        )),
                        None,
                    ) => Granted::Inherited(name.to_owned()),
                    (Some((first, _)), Some((second, _))) => {
                        problems.push(ProxyProblem::AgentModels {
                            subject: subject.clone(),
                            agent: agent.to_string(),
                            models: [first, second]
                                .into_iter()
                                .chain(served.map(|(name, _)| name))
                                .map(str::to_owned)
                                .collect(),
                        });
                        continue;
                    }
                }
            }
        };
        grants.insert(subject.clone(), (agent.clone(), granted));
    }
    if problems.is_empty() {
        Ok(grants)
    } else {
        Err(problems)
    }
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
    #[error("proxy model {model:?} settings could not be encoded")]
    Settings { model: String },
    #[error(transparent)]
    Client(#[from] dekopon_model_proxy::ClientError),
    #[error(transparent)]
    Listen(#[from] dekopon_model_proxy::tls::ListenError),
}

pub(crate) fn build(
    grants: &Grants,
    models: &[ModelConfig],
    credentials: &ConfiguredModels,
    metering: &Arc<Metering>,
) -> Result<Arc<ModelProxy>, ProxyStartError> {
    build_with(grants, models, credentials, metering, |variable| {
        std::env::var_os(variable)
    })
}

#[cfg(test)]
pub(crate) async fn start_with(
    proxy: &ResolvedProxy,
    grants: &Grants,
    models: &[ModelConfig],
    metering: &Arc<Metering>,
    tasks: &mut tokio::task::JoinSet<()>,
    resolve: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<std::net::SocketAddr, ProxyStartError> {
    let credentials = ConfiguredModels::default();
    let router = build_with(grants, models, &credentials, metering, resolve)?.router();
    listen(proxy, router, tasks).await
}

fn build_with(
    grants: &Grants,
    models: &[ModelConfig],
    credentials: &ConfiguredModels,
    metering: &Arc<Metering>,
    resolve: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Arc<ModelProxy>, ProxyStartError> {
    let granted = grants
        .values()
        .flat_map(|(_, granted)| match granted {
            Granted::Listed(models) => models.iter().map(String::as_str).collect::<Vec<_>>(),
            Granted::Inherited(model) => vec![model.as_str()],
        })
        .collect::<BTreeSet<_>>();
    let mut upstreams = Vec::new();
    for model in models.iter().filter(|model| granted.contains(model.name())) {
        let upstream = match model {
            ModelConfig::Anthropic {
                name, api_key_env, ..
            } => Upstream::Anthropic {
                endpoint: ANTHROPIC_ENDPOINT.to_owned(),
                api_key: Redacted::new(model_credential(name, api_key_env, resolve(api_key_env))?),
            },
            ModelConfig::Openrouter {
                name,
                api_key_env,
                generation,
                reasoning,
                routing,
                cache,
                ..
            } => Upstream::OpenRouter {
                endpoint: OPENROUTER_ENDPOINT.to_owned(),
                api_key: Redacted::new(model_credential(name, api_key_env, resolve(api_key_env))?),
                settings: dekopon_model::openrouter::settings::Settings {
                    generation: generation.clone(),
                    reasoning: reasoning.clone(),
                    routing: routing.clone(),
                    cache: cache.clone(),
                }
                .request_fields()
                .map_err(|_unencodable| ProxyStartError::Settings {
                    model: name.clone(),
                })?,
            },
            ModelConfig::ChatgptSubscription {
                name,
                auth_file,
                timeout_ms,
                ..
            } => Upstream::Codex {
                endpoint: CODEX_ENDPOINT.to_owned(),
                refresh: dekopon_model::chatgpt::RefreshClient::new(
                    std::time::Duration::from_millis(*timeout_ms),
                    model.ca_bundles(),
                )
                .map_err(|source| ProxyStartError::Codex {
                    model: name.clone(),
                    source: Box::new(source),
                })?,
                credential: credentials
                    .chatgpt_credential(auth_file.as_deref())
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
    let guests = grants
        .iter()
        .map(|(subject, (agent, granted))| {
            (
                subject.clone(),
                Grant {
                    agent: agent.clone(),
                    models: granted.clone(),
                },
            )
        })
        .collect::<HashMap<_, _>>();
    Ok(Arc::new(ModelProxy::new(
        upstreams,
        guests,
        Arc::clone(metering),
    )?))
}

pub(crate) async fn listen(
    proxy: &ResolvedProxy,
    router: axum::Router,
    tasks: &mut tokio::task::JoinSet<()>,
) -> Result<std::net::SocketAddr, ProxyStartError> {
    let listener =
        Listener::bind(proxy.bind, proxy.files.clone(), proxy.jail_identity.clone()).await?;
    let address =
        listener
            .local_addr()
            .map_err(|source| dekopon_model_proxy::tls::ListenError::Bind {
                bind: proxy.bind,
                source,
            })?;
    tasks.spawn(listener.serve(router, proxy.max_connections, std::future::pending()));
    tracing::info!(
        event = "gateway_proxy_listening",
        proxy.port = address.port(),
        "gateway.proxy.listening"
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
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use dekopon_broker_protocol::BrokerSocketDiscovery;
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, SanType};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::{
        config::{ConfigError, ConfigProblem, GatewaydConfig},
        routes::RouteProblem,
    };

    pub(crate) const JAIL: &str = "spiffe://homelab/ns/vm-runner/sa/vm-runner-jail";

    pub(crate) struct Pki {
        pub(crate) directory: tempfile::TempDir,
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

    pub(crate) fn pki() -> Pki {
        let directory = tempfile::TempDir::new().unwrap();
        let server_ca = issuer();
        let jail_ca = issuer();
        let (cert, key) = leaf(&server_ca, &["dekopon-gatewayd.test"], None);
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
            "apiVersion: dekopon.dev/gatewayd/v1alpha1\n\
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
        let config = serde_yaml::from_str::<GatewaydConfig>(document).expect("the fixture decodes");
        crate::config::resolve(
            config,
            PathBuf::from("/tmp/gatewayd.yaml"),
            &BrokerSocketDiscovery::new(None, None, Some(PathBuf::from("/run/user/501")), None),
            501,
            crate::config::LoadMode::Boot,
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
        let (agent, guest) = &proxy.guests["dekopon:gylmar-vm"];
        assert_eq!(agent.as_str(), "gylmar");
        assert_eq!(
            guest,
            &Guest::Listed(BTreeSet::from([
                "claude-opus".to_owned(),
                "glm-flash".to_owned()
            ]))
        );
        assert_eq!(proxy.max_connections, 16);

        let root = tempfile::tempdir().unwrap();
        let mounted = root.path().join("etc/dekopon-proxy-tls");
        std::fs::create_dir_all(&mounted).unwrap();
        for file in ["tls.crt", "tls.key", "ca.crt"] {
            std::fs::copy(pki.directory.path().join(file), mounted.join(file)).unwrap();
        }
        let candidate = resolved(&document(
            &mounted,
            "  guests:\n    'dekopon:gylmar-vm': {agent: gylmar, models: [claude-opus]}\n",
        ))
        .unwrap();
        let files = candidate.proxy.unwrap().files;
        assert_eq!(files.cert, mounted.join("tls.crt"));
        assert_eq!(files.key, mounted.join("tls.key"));
        assert_eq!(files.client_ca, mounted.join("ca.crt"));
    }

    #[test]
    fn every_proxy_problem_is_reported_at_once() {
        let pki = pki();
        std::fs::remove_file(pki.directory.path().join("ca.crt")).unwrap();
        let document = document(
            pki.directory.path(),
            "  maxConnections: 0\n  guests:\n    'dekopon:ghost-vm': {agent: ghost, models: [nope, local]}\n",
        )
        .replace("agent: gylmar}", "agent: gylmar, model: claude-opus}");
        let Err(ConfigError::Invalid { problems, .. }) = resolved(&document) else {
            panic!("the proxy block is refused");
        };
        let expected: [fn(&ConfigProblem) -> bool; 6] = [
            |problem| matches!(problem, ConfigProblem::Proxy(ProxyProblem::UnknownAgent { subject }) if subject == "dekopon:ghost-vm"),
            |problem| matches!(problem, ConfigProblem::Proxy(ProxyProblem::UnknownModel { subject, position: 0 }) if subject == "dekopon:ghost-vm"),
            |problem| matches!(problem, ConfigProblem::Proxy(ProxyProblem::UnservableModel { subject, model }) if subject == "dekopon:ghost-vm" && model == "local"),
            |problem| matches!(problem, ConfigProblem::Proxy(ProxyProblem::Tls(_))),
            |problem| matches!(problem, ConfigProblem::ProxyOnlyRouteModel { model } if model == "claude-opus"),
            |problem| matches!(problem, ConfigProblem::Proxy(ProxyProblem::NoConnections)),
        ];
        for expected in expected {
            assert!(problems.iter().any(expected), "{problems:?}");
        }
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
            .connect("dekopon-gatewayd.test".try_into().unwrap(), stream)
            .await
            .unwrap();
        tls.write_all(
            format!(
                "POST /v1/messages HTTP/1.1\r\nhost: dekopon-gatewayd.test\r\nx-dekopon-vm-subject: {subject}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
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
            &grants(config.proxy.as_ref(), &RoutingTable::default()).unwrap(),
            &config.models,
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
    fn gylmar() -> dekopon_config::LocalCatalog {
        dekopon_config::LocalCatalog::from_str(
            Path::new("dekopon.yaml"),
            &crate::tests::catalog_text(true, None).replace("reviewer", "gylmar"),
        )
        .unwrap()
    }

    const INHERITING: &str = "  guests:\n    'dekopon:gylmar-vm': {agent: gylmar}\n";

    #[test]
    fn an_agent_whose_routes_resolve_to_several_models_cannot_be_inherited() {
        let pki = pki();
        let document = document(pki.directory.path(), INHERITING)
            .replace(
                "[directMessage]}, agent: gylmar}",
                "[directMessage]}, agent: gylmar, model: glm-flash}\n- {transport: dev, conversation: {kind: [directMessage]}, agent: gylmar, model: local}",
            );
        let config = resolved(&document).unwrap();
        let Err(error) = RoutingTable::bind(&config, &gylmar()) else {
            panic!("the inheriting guest is refused");
        };
        assert!(
            error.problems.iter().any(|problem| matches!(
                problem,
                RouteProblem::Proxy(ProxyProblem::AgentModels { subject, agent, models })
                    if subject == "dekopon:gylmar-vm"
                        && agent == "gylmar"
                        && models == &["glm-flash".to_owned(), "local".to_owned()]
            )),
            "{:?}",
            error.problems
        );
    }

    #[tokio::test]
    async fn the_started_listener_serves_an_inheriting_guest_its_agents_model_only() {
        let pki = pki();
        let config = resolved(&document(pki.directory.path(), INHERITING).replace(
            "[directMessage]}, agent: gylmar}",
            "[directMessage]}, agent: gylmar, model: glm-flash}",
        ))
        .unwrap();
        let routes = RoutingTable::bind(&config, &gylmar()).unwrap();
        assert_eq!(
            routes.guests()["dekopon:gylmar-vm"].1,
            Granted::Inherited("glm-flash".to_owned())
        );
        let metering = Arc::new(Metering::new(Vec::new(), Metering::system_clock()));
        let mut tasks = tokio::task::JoinSet::new();
        let address = start_with(
            config.proxy.as_ref().unwrap(),
            routes.guests(),
            &config.models,
            &metering,
            &mut tasks,
            |_| Some("synthetic-key".into()),
        )
        .await
        .unwrap();
        let response = ask(
            address,
            &pki,
            "dekopon:gylmar-vm",
            r#"{"model":"claude-opus"}"#,
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(
            response.contains(
                "dekopon sandbox: this VM runs on its agent's model `glm-flash`, which is served on `/v1/chat/completions`, not `/v1/messages`."
            ),
            "{response}"
        );
        tasks.shutdown().await;
    }
    #[tokio::test]
    async fn check_warns_once_for_a_guest_that_lists_models() {
        let pki = pki();
        let directory = pki.directory.path();
        let config = document(
            directory,
            "  guests:\n    'dekopon:gylmar-vm': {agent: gylmar, models: [glm-flash]}\n    'dekopon:gylmar-ci': {agent: gylmar}\n",
        )
        .replace(
            "[directMessage]}, agent: gylmar}",
            "[directMessage]}, agent: gylmar, model: glm-flash}",
        );
        let path = directory.join("gatewayd.yaml");
        std::fs::write(&path, config).unwrap();
        std::fs::write(
            directory.join("dekopon.yaml"),
            crate::tests::catalog_text(true, None).replace("reviewer", "gylmar"),
        )
        .unwrap();
        let report = crate::check(&path, None, None).await;
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        let deprecated = report
            .warnings
            .iter()
            .filter(|warning| matches!(warning, crate::CheckWarning::GuestModels { .. }))
            .collect::<Vec<_>>();
        assert!(
            matches!(deprecated[..], [crate::CheckWarning::GuestModels { subject }] if subject == "dekopon:gylmar-vm"),
            "{deprecated:?}"
        );
    }
}
