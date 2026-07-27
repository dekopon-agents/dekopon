//! Local Dekopon configuration discovery, loading, and validation.
//!
//! YAML and JSON are accepted through the same parser. A file may contain one resource,
//! a YAML sequence of resources, or multiple YAML documents. Parsing happens once into a
//! [`LocalCatalog`], after which consumers operate only on typed protocol resources.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
};

use dekopon_core::{AgentId, CapabilityId, IdentifierError, ProviderId};
use dekopon_protocol::{Agent, ApiVersion, Capability, Kind, Provider};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use thiserror::Error;

/// Environment variable used for an explicit configuration path.
pub const CONFIG_ENV: &str = "DEKOPON_CONFIG";

/// A validated, deterministically ordered local resource catalog.
#[derive(Clone, Debug)]
pub struct LocalCatalog {
    source: PathBuf,
    agents: BTreeMap<AgentId, Agent>,
    capabilities: BTreeMap<CapabilityId, Capability>,
    providers: BTreeMap<ProviderId, Provider>,
}

impl LocalCatalog {
    /// Reads, parses, and validates a YAML or JSON catalog.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_str(path, &contents)
    }

    /// Parses and validates catalog text while retaining a source name for diagnostics.
    pub fn from_str(source: impl AsRef<Path>, contents: &str) -> Result<Self, ConfigError> {
        let source = source.as_ref().to_path_buf();
        let source_name = source.display().to_string();
        let mut resources = Vec::new();

        for (document_index, document) in serde_yaml::Deserializer::from_str(contents).enumerate() {
            let document_number = document_index + 1;
            let value = Value::deserialize(document).map_err(|error| {
                let location = error.location().map_or_else(String::new, |location| {
                    format!(" at line {}, column {}", location.line(), location.column())
                });
                ConfigError::Parse {
                    path: source_name.clone(),
                    origin: format!("document {document_number}{location}"),
                    source: error,
                }
            })?;

            match value {
                Value::Null => {}
                Value::Sequence(items) => {
                    for (item_index, item) in items.into_iter().enumerate() {
                        resources.push((
                            format!("document {document_number}, item {}", item_index + 1),
                            item,
                        ));
                    }
                }
                other => resources.push((format!("document {document_number}"), other)),
            }
        }

        if resources.is_empty() {
            return Err(ConfigError::Empty { path: source_name });
        }

        let mut agents = BTreeMap::<AgentId, (String, Agent)>::new();
        let mut capabilities = BTreeMap::<CapabilityId, (String, Capability)>::new();
        let mut providers = BTreeMap::<ProviderId, (String, Provider)>::new();

        for (origin, value) in resources {
            let kind = resource_kind(&value).map(str::to_owned).ok_or_else(|| {
                ConfigError::MissingKind {
                    path: source.display().to_string(),
                    origin: origin.clone(),
                }
            })?;

            match kind.as_str() {
                "Agent" => {
                    let resource = decode::<Agent>(&source, &origin, &kind, value)?;
                    validate_header(
                        &source,
                        &origin,
                        Kind::Agent,
                        resource.api_version,
                        resource.kind,
                    )?;
                    let id =
                        parse_id::<AgentId>(&source, &origin, "Agent", &resource.metadata.name)?;
                    if let Some((first, _)) = agents.get(&id) {
                        return Err(ConfigError::DuplicateResource {
                            path: source.display().to_string(),
                            kind: "Agent",
                            name: id.to_string(),
                            first: first.clone(),
                            duplicate: origin,
                        });
                    }
                    agents.insert(id, (origin, resource));
                }
                "Capability" => {
                    let resource = decode::<Capability>(&source, &origin, &kind, value)?;
                    validate_header(
                        &source,
                        &origin,
                        Kind::Capability,
                        resource.api_version,
                        resource.kind,
                    )?;
                    let id = parse_id::<CapabilityId>(
                        &source,
                        &origin,
                        "Capability",
                        &resource.metadata.name,
                    )?;
                    if let Some((first, _)) = capabilities.get(&id) {
                        return Err(ConfigError::DuplicateResource {
                            path: source.display().to_string(),
                            kind: "Capability",
                            name: id.to_string(),
                            first: first.clone(),
                            duplicate: origin,
                        });
                    }
                    capabilities.insert(id, (origin, resource));
                }
                "Provider" => {
                    let resource = decode::<Provider>(&source, &origin, &kind, value)?;
                    validate_header(
                        &source,
                        &origin,
                        Kind::Provider,
                        resource.api_version,
                        resource.kind,
                    )?;
                    let id = parse_id::<ProviderId>(
                        &source,
                        &origin,
                        "Provider",
                        &resource.metadata.name,
                    )?;
                    if let Some((first, _)) = providers.get(&id) {
                        return Err(ConfigError::DuplicateResource {
                            path: source.display().to_string(),
                            kind: "Provider",
                            name: id.to_string(),
                            first: first.clone(),
                            duplicate: origin,
                        });
                    }
                    providers.insert(id, (origin, resource));
                }
                unsupported => {
                    return Err(ConfigError::UnsupportedKind {
                        path: source.display().to_string(),
                        origin,
                        kind: unsupported.to_owned(),
                    });
                }
            }
        }

        validate_references(&source, &agents, &capabilities, &providers)?;

        Ok(Self {
            source,
            agents: agents
                .into_iter()
                .map(|(id, (_, resource))| (id, resource))
                .collect(),
            capabilities: capabilities
                .into_iter()
                .map(|(id, (_, resource))| (id, resource))
                .collect(),
            providers: providers
                .into_iter()
                .map(|(id, (_, resource))| (id, resource))
                .collect(),
        })
    }

    /// Source file from which the catalog was loaded.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Agents in deterministic identifier order.
    pub fn agents(&self) -> impl ExactSizeIterator<Item = &Agent> {
        self.agents.values()
    }

    /// Capabilities in deterministic identifier order.
    pub fn capabilities(&self) -> impl ExactSizeIterator<Item = &Capability> {
        self.capabilities.values()
    }

    /// Providers in deterministic identifier order.
    pub fn providers(&self) -> impl ExactSizeIterator<Item = &Provider> {
        self.providers.values()
    }

    /// Looks up an agent by validated identifier.
    #[must_use]
    pub fn agent(&self, id: &AgentId) -> Option<&Agent> {
        self.agents.get(id)
    }

    /// Looks up a capability by validated identifier.
    #[must_use]
    pub fn capability(&self, id: &CapabilityId) -> Option<&Capability> {
        self.capabilities.get(id)
    }

    /// Looks up a provider by validated identifier.
    #[must_use]
    pub fn provider(&self, id: &ProviderId) -> Option<&Provider> {
        self.providers.get(id)
    }

    /// Creates an owned, serializable view for `dekopon config view`.
    #[must_use]
    pub fn snapshot(&self) -> CatalogSnapshot {
        CatalogSnapshot {
            api_version: ApiVersion::V1Alpha1,
            agents: self.agents().cloned().collect(),
            capabilities: self.capabilities().cloned().collect(),
            providers: self.providers().cloned().collect(),
        }
    }
}

/// Canonical serializable view of a local catalog.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CatalogSnapshot {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Agents ordered by name.
    pub agents: Vec<Agent>,
    /// Capabilities ordered by name.
    pub capabilities: Vec<Capability>,
    /// Providers ordered by name.
    pub providers: Vec<Provider>,
}

fn resource_kind(value: &Value) -> Option<&str> {
    let mapping = value.as_mapping()?;
    mapping
        .get(Value::String("kind".to_owned()))
        .and_then(Value::as_str)
}

fn decode<T>(path: &Path, origin: &str, kind: &str, value: Value) -> Result<T, ConfigError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_yaml::from_value(value).map_err(|source| ConfigError::Decode {
        path: path.display().to_string(),
        origin: origin.to_owned(),
        kind: kind.to_owned(),
        source,
    })
}

fn parse_id<T>(path: &Path, origin: &str, kind: &'static str, name: &str) -> Result<T, ConfigError>
where
    T: std::str::FromStr<Err = IdentifierError>,
{
    name.parse().map_err(|source| ConfigError::InvalidName {
        path: path.display().to_string(),
        origin: origin.to_owned(),
        kind,
        name: name.to_owned(),
        source: Box::new(source),
    })
}

fn validate_header(
    path: &Path,
    origin: &str,
    expected: Kind,
    api_version: ApiVersion,
    actual: Kind,
) -> Result<(), ConfigError> {
    if api_version != ApiVersion::V1Alpha1 {
        return Err(ConfigError::UnsupportedApiVersion {
            path: path.display().to_string(),
            origin: origin.to_owned(),
            version: api_version.to_string(),
        });
    }
    if actual != expected {
        return Err(ConfigError::KindMismatch {
            path: path.display().to_string(),
            origin: origin.to_owned(),
            expected,
            actual,
        });
    }
    Ok(())
}

fn validate_references(
    path: &Path,
    agents: &BTreeMap<AgentId, (String, Agent)>,
    capabilities: &BTreeMap<CapabilityId, (String, Capability)>,
    providers: &BTreeMap<ProviderId, (String, Provider)>,
) -> Result<(), ConfigError> {
    let path = path.display().to_string();

    for (agent_id, (_, agent)) in agents {
        for capability in &agent.spec.capabilities {
            if !capabilities.contains_key(capability) {
                return Err(ConfigError::MissingCapability {
                    path: path.clone(),
                    agent: agent_id.to_string(),
                    capability: capability.to_string(),
                });
            }
        }
        for provider in &agent.spec.providers {
            if !providers.contains_key(provider) {
                return Err(ConfigError::MissingProvider {
                    path: path.clone(),
                    resource_kind: "agent",
                    resource: agent_id.to_string(),
                    provider: provider.to_string(),
                });
            }
        }
    }

    for (capability_id, (_, capability)) in capabilities {
        if !providers.contains_key(&capability.spec.provider) {
            return Err(ConfigError::MissingProvider {
                path: path.clone(),
                resource_kind: "capability",
                resource: capability_id.to_string(),
                provider: capability.spec.provider.to_string(),
            });
        }
    }

    Ok(())
}

/// Inputs used to resolve the configuration discovery precedence.
///
/// The explicit CLI and environment paths are authoritative even when they do not exist,
/// so users receive a direct read error instead of an unexpected fallback. Default paths
/// are selected only when an existing regular file is found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryContext {
    explicit: Option<PathBuf>,
    environment: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
    current_directory: PathBuf,
}

impl DiscoveryContext {
    /// Captures discovery inputs from the current process.
    pub fn from_process(explicit: Option<PathBuf>) -> Result<Self, ConfigError> {
        Ok(Self {
            explicit,
            environment: env::var_os(CONFIG_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            xdg_config_home: env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            home: env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            current_directory: env::current_dir().map_err(ConfigError::CurrentDirectory)?,
        })
    }

    /// Creates an injectable discovery context for deterministic callers and tests.
    #[must_use]
    pub fn new(
        explicit: Option<PathBuf>,
        environment: Option<PathBuf>,
        xdg_config_home: Option<PathBuf>,
        home: Option<PathBuf>,
        current_directory: PathBuf,
    ) -> Self {
        Self {
            explicit,
            environment,
            xdg_config_home,
            home,
            current_directory,
        }
    }

    /// Resolves the highest-precedence configuration path.
    pub fn resolve(&self) -> Result<PathBuf, ConfigError> {
        if let Some(path) = &self.explicit {
            return Ok(path.clone());
        }
        if let Some(path) = &self.environment {
            return Ok(path.clone());
        }

        let mut searched = Vec::new();
        if let Some(root) = &self.xdg_config_home {
            searched.push(root.join("dekopon/config.yaml"));
        }
        if let Some(home) = &self.home {
            searched.push(home.join(".config/dekopon/config.yaml"));
        }
        searched.push(self.current_directory.join("dekopon.yaml"));

        if let Some(path) = searched.iter().find(|path| path.is_file()) {
            return Ok(path.clone());
        }

        Err(ConfigError::NotFound {
            searched: searched
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        })
    }
}

/// Discovers, reads, and validates configuration using process inputs.
pub fn load_discovered(explicit: Option<PathBuf>) -> Result<LocalCatalog, ConfigError> {
    let path = DiscoveryContext::from_process(explicit)?.resolve()?;
    LocalCatalog::load(path)
}

/// Configuration discovery, parse, or validation failure.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// An authoritative path could not be read.
    #[error("failed to read configuration {path}: {source}")]
    Read {
        /// Display path.
        path: String,
        /// Underlying file-system error.
        #[source]
        source: io::Error,
    },
    /// The YAML/JSON stream could not be parsed.
    #[error("{path}: {origin}: invalid YAML or JSON: {source}")]
    Parse {
        /// Display path.
        path: String,
        /// Document and source location.
        origin: String,
        /// Parser diagnostic.
        #[source]
        source: serde_yaml::Error,
    },
    /// A typed resource could not be decoded.
    #[error("{path}: {origin}: invalid {kind}: {source}")]
    Decode {
        /// Display path.
        path: String,
        /// Document location.
        origin: String,
        /// Authored kind.
        kind: String,
        /// Typed decoder diagnostic.
        #[source]
        source: serde_yaml::Error,
    },
    /// No resources were authored.
    #[error("{path}: configuration contains no resources")]
    Empty {
        /// Display path.
        path: String,
    },
    /// A resource omitted its discriminator.
    #[error("{path}: {origin}: resource is missing string field `kind`")]
    MissingKind {
        /// Display path.
        path: String,
        /// Document location.
        origin: String,
    },
    /// The authored resource kind is not implemented.
    #[error("{path}: {origin}: unsupported resource kind {kind:?}")]
    UnsupportedKind {
        /// Display path.
        path: String,
        /// Document location.
        origin: String,
        /// Authored kind.
        kind: String,
    },
    /// The resource decoder and its kind field disagreed.
    #[error("{path}: {origin}: expected kind {expected}, found {actual}")]
    KindMismatch {
        /// Display path.
        path: String,
        /// Document location.
        origin: String,
        /// Decoder-selected kind.
        expected: Kind,
        /// Authored kind field.
        actual: Kind,
    },
    /// A future API version reached semantic validation.
    #[error("{path}: {origin}: unsupported API version {version}")]
    UnsupportedApiVersion {
        /// Display path.
        path: String,
        /// Document location.
        origin: String,
        /// Authored API version.
        version: String,
    },
    /// Resource metadata contained an invalid kind-specific identifier.
    #[error("{path}: {origin}: invalid {kind} name {name:?}: {source}")]
    InvalidName {
        /// Display path.
        path: String,
        /// Document location.
        origin: String,
        /// Resource kind.
        kind: &'static str,
        /// Invalid name.
        name: String,
        /// Identifier diagnostic.
        #[source]
        source: Box<IdentifierError>,
    },
    /// Two resources of the same kind used one name.
    #[error("{path}: duplicate {kind} {name:?} at {duplicate}; first declared at {first}")]
    DuplicateResource {
        /// Display path.
        path: String,
        /// Resource kind.
        kind: &'static str,
        /// Duplicate name.
        name: String,
        /// First declaration.
        first: String,
        /// Duplicate declaration.
        duplicate: String,
    },
    /// An agent referenced a capability not present in the catalog.
    #[error("{path}: agent {agent:?} references missing capability {capability:?}")]
    MissingCapability {
        /// Display path.
        path: String,
        /// Agent name.
        agent: String,
        /// Missing capability name.
        capability: String,
    },
    /// An agent or capability referenced a provider not present in the catalog.
    #[error("{path}: {resource_kind} {resource:?} references missing provider {provider:?}")]
    MissingProvider {
        /// Display path.
        path: String,
        /// Referencing resource kind.
        resource_kind: &'static str,
        /// Referencing resource name.
        resource: String,
        /// Missing provider name.
        provider: String,
    },
    /// No default candidate exists.
    #[error("no Dekopon configuration found; searched: {searched}")]
    NotFound {
        /// Comma-separated paths in precedence order.
        searched: String,
    },
    /// The process current directory was unavailable.
    #[error("could not resolve the current directory: {0}")]
    CurrentDirectory(#[source] io::Error),
}

#[cfg(test)]
mod tests;
