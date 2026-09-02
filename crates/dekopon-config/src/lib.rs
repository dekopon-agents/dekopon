//! Local Dekopon configuration discovery, loading, and validation.
//!
//! YAML and JSON are accepted through the same parser. A file may contain one resource,
//! a YAML sequence of resources, or multiple YAML documents. Parsing happens once into a
//! [`LocalCatalog`], after which consumers operate only on typed protocol resources.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
};

use dekopon_core::{AgentId, CapabilityId, IdentifierError, ProviderId};
use dekopon_protocol::{Agent, ApiVersion, Capability, Provider};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use thiserror::Error;

pub mod skill;

pub use skill::{Skill, SkillError, SkillResource, load_skill};

/// Environment variable used for an explicit configuration path.
pub const CONFIG_ENV: &str = "DEKOPON_CONFIG";

/// A validated, deterministically ordered local resource catalog.
#[derive(Clone, Debug)]
pub struct LocalCatalog {
    source: PathBuf,
    agents: BTreeMap<AgentId, Agent>,
    capabilities: BTreeMap<CapabilityId, Capability>,
    providers: BTreeMap<ProviderId, Provider>,
    /// Every agent's mounted skills, in the order its `spec.skills` names them.
    ///
    /// Read once here, at load, so a session never touches the filesystem to show a model a
    /// skill, and so a skill that cannot be read refuses the catalog instead of a session.
    skills: BTreeMap<AgentId, Vec<Skill>>,
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

        let mut agents = ResourceSet::<Agent>::default();
        let mut capabilities = ResourceSet::<Capability>::default();
        let mut providers = ResourceSet::<Provider>::default();
        let mut problems = Vec::new();
        // A resource that never reached its set cannot be referenced by name, so reference
        // checks below would blame the resources pointing at it as well. Report the real
        // failure alone rather than twice.
        let mut incomplete = false;

        for (origin, value) in resources {
            let outcome = match string_field(&value, "kind").map(str::to_owned) {
                Some(kind) => match kind.as_str() {
                    Agent::KIND => agents.insert(&origin, value),
                    Capability::KIND => capabilities.insert(&origin, value),
                    Provider::KIND => providers.insert(&origin, value),
                    _ => Err(CatalogProblem::UnsupportedKind { origin, kind }),
                },
                None => Err(CatalogProblem::MissingKind { origin }),
            };
            if let Err(problem) = outcome {
                incomplete |= problem.drops_resource();
                problems.push(problem);
            }
        }

        if !incomplete {
            validate_references(&agents, &capabilities, &providers, &mut problems);
        }
        // Skills reference files rather than other resources, so an agent that decoded can have
        // its skills checked whatever happened to the rest of the catalog. Relative paths resolve
        // against the catalog file's own directory, the rule `dekopond` applies to its own paths.
        let base = source.parent().map(Path::to_path_buf).unwrap_or_default();
        let skills = load_agent_skills(&agents, &base, &mut problems);

        if !problems.is_empty() {
            return Err(ConfigError::Invalid {
                path: source_name,
                problems,
            });
        }

        Ok(Self {
            source,
            agents: agents.into_map(),
            capabilities: capabilities.into_map(),
            providers: providers.into_map(),
            skills,
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

    /// The skills one agent mounts, in the order its `spec.skills` names them.
    ///
    /// Empty for an agent that mounts none and for an identifier the catalog does not declare;
    /// [`Self::agent`] is the question "does this agent exist".
    #[must_use]
    pub fn agent_skills(&self, id: &AgentId) -> &[Skill] {
        self.skills.get(id).map_or(&[], Vec::as_slice)
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

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .as_mapping()?
        .get(Value::String(field.to_owned()))
        .and_then(Value::as_str)
}

/// One authored resource kind, decoded and keyed by its own identifier type.
trait Resource: Sized + for<'de> Deserialize<'de> {
    /// Validated identifier type this kind is stored under.
    type Id: FromStr<Err = IdentifierError> + Ord + fmt::Display;

    /// Authored `kind` discriminator.
    const KIND: &'static str;

    /// Authored metadata name, before identifier validation.
    fn name(&self) -> &str;
}

impl Resource for Agent {
    type Id = AgentId;
    const KIND: &'static str = "Agent";

    fn name(&self) -> &str {
        &self.metadata.name
    }
}

impl Resource for Capability {
    type Id = CapabilityId;
    const KIND: &'static str = "Capability";

    fn name(&self) -> &str {
        &self.metadata.name
    }
}

impl Resource for Provider {
    type Id = ProviderId;
    const KIND: &'static str = "Provider";

    fn name(&self) -> &str {
        &self.metadata.name
    }
}

/// Resources of one kind, with the document each was declared in.
struct ResourceSet<T: Resource> {
    entries: BTreeMap<T::Id, (String, T)>,
}

impl<T: Resource> Default for ResourceSet<T> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

impl<T: Resource> ResourceSet<T> {
    /// Decodes, validates, and admits one authored document, or reports why it cannot join.
    fn insert(&mut self, origin: &str, value: Value) -> Result<(), CatalogProblem> {
        // The typed decoder knows exactly one API version, so a future one would fail here as an
        // unknown enum variant. Reading the authored string first is what keeps the dedicated
        // message reachable.
        if let Some(version) = string_field(&value, "apiVersion")
            && version != ApiVersion::V1Alpha1.to_string()
        {
            return Err(CatalogProblem::UnsupportedApiVersion {
                origin: origin.to_owned(),
                version: version.to_owned(),
            });
        }

        let resource =
            serde_yaml::from_value::<T>(value).map_err(|source| CatalogProblem::Decode {
                origin: origin.to_owned(),
                kind: T::KIND,
                source,
            })?;
        let id =
            resource
                .name()
                .parse::<T::Id>()
                .map_err(|source| CatalogProblem::InvalidName {
                    origin: origin.to_owned(),
                    kind: T::KIND,
                    name: resource.name().to_owned(),
                    source: Box::new(source),
                })?;
        if let Some((first, _)) = self.entries.get(&id) {
            return Err(CatalogProblem::DuplicateResource {
                kind: T::KIND,
                name: id.to_string(),
                first: first.clone(),
                duplicate: origin.to_owned(),
            });
        }
        self.entries.insert(id, (origin.to_owned(), resource));
        Ok(())
    }

    fn contains(&self, id: &T::Id) -> bool {
        self.entries.contains_key(id)
    }

    fn get(&self, id: &T::Id) -> Option<&T> {
        self.entries.get(id).map(|(_, resource)| resource)
    }

    fn iter(&self) -> impl Iterator<Item = (&T::Id, &T)> {
        self.entries
            .iter()
            .map(|(id, (_, resource))| (id, resource))
    }

    fn into_map(self) -> BTreeMap<T::Id, T> {
        self.entries
            .into_iter()
            .map(|(id, (_, resource))| (id, resource))
            .collect()
    }
}

fn validate_references(
    agents: &ResourceSet<Agent>,
    capabilities: &ResourceSet<Capability>,
    providers: &ResourceSet<Provider>,
    problems: &mut Vec<CatalogProblem>,
) {
    for (agent_id, agent) in agents.iter() {
        // Which provider each capability routes to, so the agent's own provider list can be held
        // to the capabilities it actually declares rather than merely to existing names.
        let mut required = BTreeMap::<&ProviderId, &CapabilityId>::new();
        let mut every_capability_resolved = true;
        for capability in &agent.spec.capabilities {
            match capabilities.get(capability) {
                Some(declared) => {
                    required
                        .entry(&declared.spec.provider)
                        .or_insert(capability);
                }
                None => {
                    every_capability_resolved = false;
                    problems.push(CatalogProblem::MissingCapability {
                        agent: agent_id.to_string(),
                        capability: capability.to_string(),
                    });
                }
            }
        }

        let listed = agent.spec.providers.iter().collect::<BTreeSet<_>>();
        for provider in &agent.spec.providers {
            if !providers.contains(provider) {
                problems.push(CatalogProblem::MissingProvider {
                    resource_kind: "agent",
                    resource: agent_id.to_string(),
                    provider: provider.to_string(),
                });
            }
        }
        for (provider, capability) in &required {
            if !listed.contains(provider) {
                problems.push(CatalogProblem::UnlistedAgentProvider {
                    agent: agent_id.to_string(),
                    provider: provider.to_string(),
                    capability: capability.to_string(),
                });
            }
        }
        // An unresolved capability hides the provider it would have required, so a listed
        // provider cannot be called unreachable until every capability is known. A provider that
        // is not in the catalog at all has already been reported once, by its real name.
        if every_capability_resolved {
            for provider in listed {
                if !required.contains_key(provider) && providers.contains(provider) {
                    problems.push(CatalogProblem::UnreachableAgentProvider {
                        agent: agent_id.to_string(),
                        provider: provider.to_string(),
                    });
                }
            }
        }
    }

    for (capability_id, capability) in capabilities.iter() {
        if !providers.contains(&capability.spec.provider) {
            problems.push(CatalogProblem::MissingProvider {
                resource_kind: "capability",
                resource: capability_id.to_string(),
                provider: capability.spec.provider.to_string(),
            });
        }
    }
}

/// Reads every skill every agent names, reporting each one that cannot be mounted.
///
/// Every problem is collected rather than the first returned, for the reason the reference checks
/// above scan the whole catalog: an operator with three broken skills fixes three and validates
/// once. A skill that fails still leaves the agent's other skills checked, and two agents naming
/// one directory read it twice rather than sharing a cache, because the catalog is loaded once per
/// process and a skill directory is small.
fn load_agent_skills(
    agents: &ResourceSet<Agent>,
    base: &Path,
    problems: &mut Vec<CatalogProblem>,
) -> BTreeMap<AgentId, Vec<Skill>> {
    let mut mounted = BTreeMap::new();
    for (agent_id, agent) in agents.iter() {
        if agent.spec.skills.is_empty() {
            continue;
        }
        let mut skills = Vec::with_capacity(agent.spec.skills.len());
        let mut names = BTreeMap::new();
        for path in &agent.spec.skills {
            let resolved = if path.is_absolute() {
                path.clone()
            } else {
                base.join(path)
            };
            let skill = match skill::load_skill(&resolved) {
                Ok(skill) => skill,
                Err(source) => {
                    problems.push(CatalogProblem::Skill {
                        agent: agent_id.to_string(),
                        path: path.display().to_string(),
                        source: Box::new(source),
                    });
                    continue;
                }
            };
            // Two directories carrying one name would give a model two `read_skill` targets it
            // cannot tell apart, so the second is refused rather than shadowing the first.
            if let Some(first) = names.insert(skill.name().clone(), path.display().to_string()) {
                problems.push(CatalogProblem::DuplicateSkill {
                    agent: agent_id.to_string(),
                    name: skill.name().to_string(),
                    first,
                    duplicate: path.display().to_string(),
                });
                continue;
            }
            skills.push(skill);
        }
        mounted.insert(agent_id.clone(), skills);
    }
    mounted
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

        for path in &searched {
            match fs::metadata(path) {
                Ok(metadata) if metadata.is_file() => return Ok(path.clone()),
                // A directory or device at a candidate path is not this location's config.
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                // Anything else — a permission or traversal failure on the parent — means an
                // existing higher-precedence config may be hidden. Falling through would load a
                // lower-precedence file, so refuse instead of guessing.
                Err(source) => {
                    return Err(ConfigError::Candidate {
                        path: path.display().to_string(),
                        source,
                    });
                }
            }
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
    /// The catalog parsed, and every semantic problem found in it is listed here.
    #[error("{path}: {}", render_problems(.problems))]
    Invalid {
        /// Display path.
        path: String,
        /// Every problem found, in document then reference order.
        problems: Vec<CatalogProblem>,
    },
    /// A default candidate path could not be examined.
    #[error("failed to examine configuration candidate {path}: {source}")]
    Candidate {
        /// Display path.
        path: String,
        /// Underlying file-system error.
        #[source]
        source: io::Error,
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

fn render_problems(problems: &[CatalogProblem]) -> String {
    let mut rendered = format!(
        "{} validation problem{} found:",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    );
    for problem in problems {
        rendered.push_str("\n  - ");
        rendered.push_str(&problem.to_string());
    }
    rendered
}

/// One semantic problem in an otherwise parseable catalog.
///
/// A catalog is scanned to the end before it is refused, so an operator fixing three mistakes
/// runs `dekopon validate` once rather than three times. Problems are reported through
/// [`ConfigError::Invalid`], which owns the source path they all share.
#[derive(Debug, Error)]
pub enum CatalogProblem {
    /// A resource omitted its discriminator.
    #[error("{origin}: resource is missing string field `kind`")]
    MissingKind {
        /// Document location.
        origin: String,
    },
    /// The authored resource kind is not implemented.
    #[error("{origin}: unsupported resource kind {kind:?}")]
    UnsupportedKind {
        /// Document location.
        origin: String,
        /// Authored kind.
        kind: String,
    },
    /// The authored API version is not the one this crate implements.
    #[error("{origin}: unsupported API version {version:?}")]
    UnsupportedApiVersion {
        /// Document location.
        origin: String,
        /// Authored API version.
        version: String,
    },
    /// A typed resource could not be decoded.
    #[error("{origin}: invalid {kind}: {source}")]
    Decode {
        /// Document location.
        origin: String,
        /// Authored kind.
        kind: &'static str,
        /// Typed decoder diagnostic.
        #[source]
        source: serde_yaml::Error,
    },
    /// Resource metadata contained an invalid kind-specific identifier.
    #[error("{origin}: invalid {kind} name {name:?}: {source}")]
    InvalidName {
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
    #[error("duplicate {kind} {name:?} at {duplicate}; first declared at {first}")]
    DuplicateResource {
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
    #[error("agent {agent:?} references missing capability {capability:?}")]
    MissingCapability {
        /// Agent name.
        agent: String,
        /// Missing capability name.
        capability: String,
    },
    /// An agent or capability referenced a provider not present in the catalog.
    #[error("{resource_kind} {resource:?} references missing provider {provider:?}")]
    MissingProvider {
        /// Referencing resource kind.
        resource_kind: &'static str,
        /// Referencing resource name.
        resource: String,
        /// Missing provider name.
        provider: String,
    },
    /// An agent omitted a provider its own capabilities route to.
    #[error(
        "agent {agent:?} omits provider {provider:?}, required by capability {capability:?}, \
         from spec.providers"
    )]
    UnlistedAgentProvider {
        /// Agent name.
        agent: String,
        /// Provider the capability routes to.
        provider: String,
        /// Capability requiring the provider.
        capability: String,
    },
    /// An agent listed a provider none of its capabilities route to.
    #[error("agent {agent:?} lists provider {provider:?}, which none of its capabilities route to")]
    UnreachableAgentProvider {
        /// Agent name.
        agent: String,
        /// Unreachable provider name.
        provider: String,
    },
    /// An agent named a skill directory that could not be mounted.
    #[error("agent {agent:?} mounts skill {path:?}, which could not be loaded: {source}")]
    Skill {
        /// Agent name.
        agent: String,
        /// The authored skill path.
        path: String,
        /// Why the directory was refused, naming the file.
        #[source]
        source: Box<SkillError>,
    },
    /// An agent mounted two directories carrying one skill name.
    #[error(
        "agent {agent:?} mounts skill {name:?} twice, at {first:?} and {duplicate:?}; a model could not tell them apart"
    )]
    DuplicateSkill {
        /// Agent name.
        agent: String,
        /// The repeated skill name.
        name: String,
        /// The first path carrying it.
        first: String,
        /// The second path carrying it.
        duplicate: String,
    },
}

impl CatalogProblem {
    /// Whether the problem kept a resource out of the catalog.
    const fn drops_resource(&self) -> bool {
        matches!(
            self,
            Self::MissingKind { .. }
                | Self::UnsupportedKind { .. }
                | Self::UnsupportedApiVersion { .. }
                | Self::Decode { .. }
                | Self::InvalidName { .. }
        )
    }
}

#[cfg(test)]
mod tests;
