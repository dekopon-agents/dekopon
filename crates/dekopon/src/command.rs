//! Typed command execution, independent of rendering.

use dekopon_config::CatalogSnapshot;
use dekopon_model::chatgpt::ChatGptAuthStatus;
use dekopon_protocol::{Agent, Capability, Provider};
use serde::Serialize;

use crate::{
    catalog::{CatalogError, LocalConfigReader, ResourceReader},
    cli::{Command, ConfigCommand, DescribeCommand, GetCommand},
};

/// Typed result returned by command execution.
#[derive(Clone, Debug)]
pub enum CommandResult {
    /// CLI build information.
    Version(VersionInfo),
    /// Model-account authentication state.
    Auth(ModelAuthStatus),
    /// Agent list.
    Agents(Vec<Agent>),
    /// One agent.
    Agent(Agent),
    /// Capability list.
    Capabilities(Vec<Capability>),
    /// One capability.
    Capability(Capability),
    /// Provider list.
    Providers(Vec<Provider>),
    /// One provider.
    Provider(Provider),
    /// Expanded agent detail.
    AgentDescription(AgentDescription),
    /// Successful validation summary.
    Validation(ValidationSummary),
    /// Canonical validated catalog.
    Config(CatalogSnapshot),
}

/// Machine-readable model-account authentication state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuthStatus {
    /// Model account namespace.
    pub account: &'static str,
    /// Credential file owned by Dekopon.
    pub credential_file: String,
    /// Whether credentials are present.
    pub signed_in: bool,
    /// Whether the current access token is expired.
    pub expired: bool,
}

impl ModelAuthStatus {
    pub(crate) fn chatgpt(status: ChatGptAuthStatus) -> Self {
        Self {
            account: "chatgpt",
            credential_file: status.path.display().to_string(),
            signed_in: status.signed_in,
            expired: status.expired,
        }
    }
}

/// Machine-readable CLI version information.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    /// Product name.
    pub product: &'static str,
    /// Semantic package version.
    pub version: &'static str,
    /// Resource API version.
    pub api_version: &'static str,
}

/// Expanded agent plus its resolved declarations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDescription {
    /// Agent resource.
    pub agent: Agent,
    /// Resolved capabilities in deterministic name order.
    pub capabilities: Vec<Capability>,
    /// Resolved providers in deterministic name order.
    pub providers: Vec<Provider>,
}

/// Counts emitted after successful validation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationSummary {
    /// Resolved source path.
    pub source: String,
    /// Always true for an emitted summary; invalid catalogs return an error instead.
    pub valid: bool,
    /// Number of agents.
    pub agents: usize,
    /// Number of capabilities.
    pub capabilities: usize,
    /// Number of providers.
    pub providers: usize,
}

/// Creates a version result without loading configuration.
#[must_use]
pub const fn version_result() -> CommandResult {
    CommandResult::Version(VersionInfo {
        product: "dekopon",
        version: env!("CARGO_PKG_VERSION"),
        api_version: "dekopon.dev/v1alpha1",
    })
}

/// Executes a command against a validated local reader.
pub fn execute(
    command: &Command,
    reader: &LocalConfigReader,
) -> Result<CommandResult, CatalogError> {
    match command {
        Command::Version => Ok(version_result()),
        Command::Auth { .. } => {
            unreachable!("auth commands are executed before catalog resolution")
        }
        Command::Get { resource } => execute_get(resource, reader),
        Command::Describe { resource } => execute_describe(resource, reader),
        Command::Validate => {
            let agents = reader.list_agents()?.len();
            let capabilities = reader.list_capabilities()?.len();
            let providers = reader.list_providers()?.len();
            Ok(CommandResult::Validation(ValidationSummary {
                source: reader.source_display(),
                valid: true,
                agents,
                capabilities,
                providers,
            }))
        }
        Command::Config {
            command: ConfigCommand::View,
        } => Ok(CommandResult::Config(reader.snapshot())),
    }
}

fn execute_get(
    command: &GetCommand,
    reader: &impl ResourceReader,
) -> Result<CommandResult, CatalogError> {
    match command {
        GetCommand::Agent { name } => reader.get_agent(name).map(CommandResult::Agent),
        GetCommand::Agents => reader.list_agents().map(CommandResult::Agents),
        GetCommand::Capability { name } => {
            reader.get_capability(name).map(CommandResult::Capability)
        }
        GetCommand::Capabilities => reader.list_capabilities().map(CommandResult::Capabilities),
        GetCommand::Provider { name } => reader.get_provider(name).map(CommandResult::Provider),
        GetCommand::Providers => reader.list_providers().map(CommandResult::Providers),
    }
}

fn execute_describe(
    command: &DescribeCommand,
    reader: &impl ResourceReader,
) -> Result<CommandResult, CatalogError> {
    match command {
        DescribeCommand::Agent { name } => {
            let agent = reader.get_agent(name)?;
            let mut capabilities = agent
                .spec
                .capabilities
                .iter()
                .map(|id| reader.get_capability(id))
                .collect::<Result<Vec<_>, _>>()?;
            let mut providers = agent
                .spec
                .providers
                .iter()
                .map(|id| reader.get_provider(id))
                .collect::<Result<Vec<_>, _>>()?;
            capabilities.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
            providers.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));

            Ok(CommandResult::AgentDescription(AgentDescription {
                agent,
                capabilities,
                providers,
            }))
        }
    }
}
