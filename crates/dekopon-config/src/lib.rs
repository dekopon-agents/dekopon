#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::{
    collections::BTreeMap,
    env, fmt, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
};

use dekopon_core::{AgentId, IdentifierError};
use dekopon_protocol::{Agent, ApiVersion};
use serde::Deserialize;
use serde_yaml::Value;
use thiserror::Error;

pub mod skill;

pub use skill::{Skill, SkillError, SkillResource, load_skill};

pub const CONFIG_ENV: &str = "DEKOPON_CONFIG";
pub const MAX_INSTRUCTIONS_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct LocalCatalog {
    source: PathBuf,
    agents: BTreeMap<AgentId, Agent>,
    /// Skills are read once at catalog load so a live session never touches the filesystem, and a
    /// broken skill fails the catalog, not a running session.
    skills: BTreeMap<AgentId, Vec<Skill>>,
}

impl LocalCatalog {
    /// A directory is read as every `*.yaml` directly inside it, in filename order; an agent named in
    /// two files is a duplicate like one named twice in a file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let read = |file: &Path| {
            fs::read_to_string(file).map_err(|source| ConfigError::Read {
                path: file.display().to_string(),
                source,
            })
        };
        if !fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
            return Self::from_str(path, &read(path)?);
        }
        let entries = fs::read_dir(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| ConfigError::Read {
                path: path.display().to_string(),
                source,
            })?;
            let file = entry.path();
            if file
                .extension()
                .is_some_and(|extension| extension == "yaml")
            {
                files.push(file);
            }
        }
        files.sort();
        let sources = files
            .iter()
            .map(|file| Ok((file.clone(), read(file)?)))
            .collect::<Result<Vec<_>, ConfigError>>()?;
        Self::from_sources(path, &sources)
    }

    pub fn from_str(source: impl AsRef<Path>, contents: &str) -> Result<Self, ConfigError> {
        let source = source.as_ref();
        Self::from_sources(source, &[(source.to_path_buf(), contents.to_owned())])
    }

    fn from_sources(source: &Path, files: &[(PathBuf, String)]) -> Result<Self, ConfigError> {
        let source = source.to_path_buf();
        let source_name = source.display().to_string();
        let many = files.len() > 1;
        let mut resources = Vec::new();

        for (file, contents) in files {
            let file_name = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let prefix = if many {
                format!("{file_name} ")
            } else {
                String::new()
            };
            for (document_index, document) in
                serde_yaml::Deserializer::from_str(contents).enumerate()
            {
                let document_number = document_index + 1;
                let value = Value::deserialize(document).map_err(|error| {
                    let location = error.location().map_or_else(String::new, |location| {
                        format!(" at line {}, column {}", location.line(), location.column())
                    });
                    ConfigError::Parse {
                        path: file.display().to_string(),
                        origin: format!("document {document_number}{location}"),
                        source: error,
                    }
                })?;

                match value {
                    Value::Null => {}
                    Value::Sequence(items) => {
                        for (item_index, item) in items.into_iter().enumerate() {
                            resources.push((
                                format!(
                                    "{prefix}document {document_number}, item {}",
                                    item_index + 1
                                ),
                                item,
                            ));
                        }
                    }
                    other => resources.push((format!("{prefix}document {document_number}"), other)),
                }
            }
        }

        if resources.is_empty() {
            return Err(ConfigError::Empty { path: source_name });
        }

        let mut agents = ResourceSet::<Agent>::default();
        let mut problems = Vec::new();

        for (origin, value) in resources {
            let outcome = match string_field(&value, "kind").map(str::to_owned) {
                Some(kind) => match kind.as_str() {
                    Agent::KIND => agents.insert(&origin, value),
                    _ => Err(CatalogProblem::UnsupportedKind { origin, kind }),
                },
                None => Err(CatalogProblem::MissingKind { origin }),
            };
            if let Err(problem) = outcome {
                problems.push(problem);
            }
        }
        let base = if many || source.is_dir() {
            source.clone()
        } else {
            source.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        let skills = load_agent_skills(&agents, &base, &mut problems);
        let mut agents = agents.into_map();
        for (id, agent) in &mut agents {
            let Some(file) = agent.spec.instructions_file.clone() else {
                continue;
            };
            if agent.spec.instructions.is_some() {
                problems.push(CatalogProblem::InstructionsTwice {
                    agent: id.to_string(),
                });
                continue;
            }
            let resolved = if file.is_absolute() {
                file.clone()
            } else {
                base.join(&file)
            };
            match skill::read_bounded_text(&resolved, MAX_INSTRUCTIONS_BYTES) {
                Ok(text) => agent.spec.instructions = Some(text),
                Err(source) => problems.push(CatalogProblem::Instructions {
                    agent: id.to_string(),
                    path: file.display().to_string(),
                    source: Box::new(source),
                }),
            }
        }

        if !problems.is_empty() {
            return Err(ConfigError::Invalid {
                path: source_name,
                problems,
            });
        }

        Ok(Self {
            source,
            agents,
            skills,
        })
    }

    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn agents(&self) -> impl ExactSizeIterator<Item = &Agent> {
        self.agents.values()
    }

    #[must_use]
    pub fn agent(&self, id: &AgentId) -> Option<&Agent> {
        self.agents.get(id)
    }

    #[must_use]
    pub fn agent_skills(&self, id: &AgentId) -> &[Skill] {
        self.skills.get(id).map_or(&[], Vec::as_slice)
    }
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .as_mapping()?
        .get(Value::String(field.to_owned()))
        .and_then(Value::as_str)
}

trait Resource: Sized + for<'de> Deserialize<'de> {
    type Id: FromStr<Err = IdentifierError> + Ord + fmt::Display;

    const KIND: &'static str;

    fn name(&self) -> &str;
}

impl Resource for Agent {
    type Id = AgentId;
    const KIND: &'static str = "Agent";

    fn name(&self) -> &str {
        &self.metadata.name
    }
}

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
    fn insert(&mut self, origin: &str, value: Value) -> Result<(), CatalogProblem> {
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
            // Two skill directories sharing one name would give a model two indistinguishable read
            // targets, so the second is refused rather than shadowing the first.
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryContext {
    explicit: Option<PathBuf>,
    environment: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
    current_directory: PathBuf,
}

impl DiscoveryContext {
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
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                // A permission or traversal error might be hiding a higher-precedence config, so
                // this refuses rather than silently falling through to a lower-precedence file.
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

pub fn load_discovered(explicit: Option<PathBuf>) -> Result<LocalCatalog, ConfigError> {
    let path = DiscoveryContext::from_process(explicit)?.resolve()?;
    LocalCatalog::load(path)
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("{path}: {origin}: invalid YAML or JSON: {source}")]
    Parse {
        path: String,
        origin: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("{path}: {origin}: invalid {kind}: {source}")]
    Decode {
        path: String,
        origin: String,
        kind: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("{path}: configuration contains no resources")]
    Empty { path: String },
    #[error("{path}: {}", render_problems(.problems))]
    Invalid {
        path: String,
        problems: Vec<CatalogProblem>,
    },
    #[error("failed to examine configuration candidate {path}: {source}")]
    Candidate {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("no Dekopon configuration found; searched: {searched}")]
    NotFound { searched: String },
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

#[derive(Debug, Error)]
pub enum CatalogProblem {
    #[error("{origin}: resource is missing string field `kind`")]
    MissingKind { origin: String },
    #[error("{origin}: unsupported resource kind {kind:?}")]
    UnsupportedKind { origin: String, kind: String },
    #[error("agent {agent} sets both instructions and instructionsFile")]
    InstructionsTwice { agent: String },
    #[error("agent {agent} instructionsFile {path}: {source}")]
    Instructions {
        agent: String,
        path: String,
        #[source]
        source: Box<SkillError>,
    },
    #[error("{origin}: unsupported API version {version:?}")]
    UnsupportedApiVersion { origin: String, version: String },
    #[error("{origin}: invalid {kind}: {source}")]
    Decode {
        origin: String,
        kind: &'static str,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("{origin}: invalid {kind} name {name:?}: {source}")]
    InvalidName {
        origin: String,
        kind: &'static str,
        name: String,
        #[source]
        source: Box<IdentifierError>,
    },
    #[error("duplicate {kind} {name:?} at {duplicate}; first declared at {first}")]
    DuplicateResource {
        kind: &'static str,
        name: String,
        first: String,
        duplicate: String,
    },
    #[error("agent {agent:?} mounts skill {path:?}, which could not be loaded: {source}")]
    Skill {
        agent: String,
        path: String,
        #[source]
        source: Box<SkillError>,
    },
    #[error(
        "agent {agent:?} mounts skill {name:?} twice, at {first:?} and {duplicate:?}; a model could not tell them apart"
    )]
    DuplicateSkill {
        agent: String,
        name: String,
        first: String,
        duplicate: String,
    },
}

#[cfg(test)]
mod tests;
