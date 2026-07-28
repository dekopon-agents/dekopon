//! Deterministic rendering for typed command results.

use dekopon_config::CatalogSnapshot;
use dekopon_protocol::{
    Agent, AgentList, AgentStatus, Capability, CapabilityList, Provider, ProviderList,
};
use serde::Serialize;
use thiserror::Error;

use crate::{
    cli::OutputFormat,
    command::{AgentDescription, CommandResult, ModelAuthStatus, ValidationSummary, VersionInfo},
};

/// Failure to serialize a typed command result.
#[derive(Debug, Error)]
pub enum RenderError {
    /// JSON serialization failed.
    #[error("could not render JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// YAML serialization failed.
    #[error("could not render YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

/// Renders one typed command result in the selected format.
pub fn render(result: &CommandResult, format: OutputFormat) -> Result<String, RenderError> {
    match result {
        CommandResult::Version(info) => render_version(info, format),
        CommandResult::Auth(status) => render_auth(status, format),
        CommandResult::Agents(agents) => render_agents(agents, format),
        CommandResult::Agent(agent) => render_agent(agent, format),
        CommandResult::Capabilities(capabilities) => render_capabilities(capabilities, format),
        CommandResult::Capability(capability) => render_capability(capability, format),
        CommandResult::Providers(providers) => render_providers(providers, format),
        CommandResult::Provider(provider) => render_provider(provider, format),
        CommandResult::AgentDescription(description) => render_description(description, format),
        CommandResult::Validation(summary) => render_validation(summary, format),
        CommandResult::Config(snapshot) => render_config(snapshot, format),
    }
}

fn render_version(info: &VersionInfo, format: OutputFormat) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(info),
        OutputFormat::Yaml => to_yaml(info),
        OutputFormat::Table | OutputFormat::Wide | OutputFormat::Name => {
            Ok(format!("{} {}", info.product, info.version))
        }
    }
}

fn render_auth(status: &ModelAuthStatus, format: OutputFormat) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(status),
        OutputFormat::Yaml => to_yaml(status),
        OutputFormat::Name => Ok(format!("auth/{}", status.account)),
        OutputFormat::Table | OutputFormat::Wide => {
            let state = if !status.signed_in {
                "not signed in"
            } else if status.expired {
                "signed in; refresh required"
            } else {
                "signed in"
            };
            Ok(table(
                &["ACCOUNT", "STATUS", "CREDENTIAL FILE"],
                &[vec![
                    cell_text(status.account),
                    state.to_owned(),
                    cell_text(&status.credential_file),
                ]],
            ))
        }
    }
}

fn render_agents(agents: &[Agent], format: OutputFormat) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(&AgentList::new(agents.to_vec())),
        OutputFormat::Yaml => to_yaml(&AgentList::new(agents.to_vec())),
        OutputFormat::Name => Ok(qualified_names(
            "agent",
            agents.iter().map(resource_name_agent),
        )),
        OutputFormat::Table => Ok(agent_table(agents, false)),
        OutputFormat::Wide => Ok(agent_table(agents, true)),
    }
}

fn render_agent(agent: &Agent, format: OutputFormat) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(agent),
        OutputFormat::Yaml => to_yaml(agent),
        OutputFormat::Name => Ok(format!("agent/{}", agent.metadata.name)),
        OutputFormat::Table => Ok(agent_table(std::slice::from_ref(agent), false)),
        OutputFormat::Wide => Ok(agent_table(std::slice::from_ref(agent), true)),
    }
}

fn render_capabilities(
    capabilities: &[Capability],
    format: OutputFormat,
) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(&CapabilityList::new(capabilities.to_vec())),
        OutputFormat::Yaml => to_yaml(&CapabilityList::new(capabilities.to_vec())),
        OutputFormat::Name => Ok(qualified_names(
            "capability",
            capabilities.iter().map(resource_name_capability),
        )),
        OutputFormat::Table => Ok(capability_table(capabilities, false)),
        OutputFormat::Wide => Ok(capability_table(capabilities, true)),
    }
}

fn render_capability(capability: &Capability, format: OutputFormat) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(capability),
        OutputFormat::Yaml => to_yaml(capability),
        OutputFormat::Name => Ok(format!("capability/{}", capability.metadata.name)),
        OutputFormat::Table => Ok(capability_table(std::slice::from_ref(capability), false)),
        OutputFormat::Wide => Ok(capability_table(std::slice::from_ref(capability), true)),
    }
}

fn render_providers(providers: &[Provider], format: OutputFormat) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(&ProviderList::new(providers.to_vec())),
        OutputFormat::Yaml => to_yaml(&ProviderList::new(providers.to_vec())),
        OutputFormat::Name => Ok(qualified_names(
            "provider",
            providers.iter().map(resource_name_provider),
        )),
        OutputFormat::Table => Ok(provider_table(providers, false)),
        OutputFormat::Wide => Ok(provider_table(providers, true)),
    }
}

fn render_provider(provider: &Provider, format: OutputFormat) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(provider),
        OutputFormat::Yaml => to_yaml(provider),
        OutputFormat::Name => Ok(format!("provider/{}", provider.metadata.name)),
        OutputFormat::Table => Ok(provider_table(std::slice::from_ref(provider), false)),
        OutputFormat::Wide => Ok(provider_table(std::slice::from_ref(provider), true)),
    }
}

fn render_description(
    description: &AgentDescription,
    format: OutputFormat,
) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(description),
        OutputFormat::Yaml => to_yaml(description),
        OutputFormat::Name => Ok(format!("agent/{}", description.agent.metadata.name)),
        OutputFormat::Table | OutputFormat::Wide => Ok(agent_description(description)),
    }
}

fn render_validation(
    summary: &ValidationSummary,
    format: OutputFormat,
) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(summary),
        OutputFormat::Yaml => to_yaml(summary),
        OutputFormat::Name => Ok("config/valid".to_owned()),
        OutputFormat::Table => Ok(format!(
            "configuration valid: {} agent(s), {} capability(ies), {} provider(s)",
            summary.agents, summary.capabilities, summary.providers
        )),
        OutputFormat::Wide => Ok(format!(
            "configuration valid: {} agent(s), {} capability(ies), {} provider(s)\nsource: {}",
            summary.agents,
            summary.capabilities,
            summary.providers,
            cell_text(&summary.source)
        )),
    }
}

fn render_config(snapshot: &CatalogSnapshot, format: OutputFormat) -> Result<String, RenderError> {
    match format {
        OutputFormat::Json => to_json(snapshot),
        OutputFormat::Yaml => to_yaml(snapshot),
        OutputFormat::Name => {
            let groups = [
                qualified_names("agent", snapshot.agents.iter().map(resource_name_agent)),
                qualified_names(
                    "capability",
                    snapshot.capabilities.iter().map(resource_name_capability),
                ),
                qualified_names(
                    "provider",
                    snapshot.providers.iter().map(resource_name_provider),
                ),
            ];
            Ok(groups
                .into_iter()
                .filter(|group| !group.is_empty())
                .collect::<Vec<_>>()
                .join("\n"))
        }
        OutputFormat::Table | OutputFormat::Wide => {
            let wide = format == OutputFormat::Wide;
            Ok([
                format!("AGENTS\n{}", agent_table(&snapshot.agents, wide)),
                format!(
                    "CAPABILITIES\n{}",
                    capability_table(&snapshot.capabilities, wide)
                ),
                format!("PROVIDERS\n{}", provider_table(&snapshot.providers, wide)),
            ]
            .join("\n\n"))
        }
    }
}

fn to_json(value: &impl Serialize) -> Result<String, RenderError> {
    serde_json::to_string_pretty(value).map_err(RenderError::Json)
}

fn to_yaml(value: &impl Serialize) -> Result<String, RenderError> {
    serde_yaml::to_string(value).map_err(RenderError::Yaml)
}

fn agent_table(agents: &[Agent], wide: bool) -> String {
    let rows = agents
        .iter()
        .map(|agent| {
            let mut row = vec![
                cell_text(&agent.metadata.name),
                effective_agent_status(agent),
                agent.spec.capabilities.len().to_string(),
            ];
            if wide {
                row.extend([
                    agent.spec.providers.len().to_string(),
                    option_cell(agent.spec.model_class.as_deref()),
                    option_cell(agent.spec.policy_profile.as_deref()),
                ]);
            }
            row.push(cell_text(&agent.spec.description));
            row
        })
        .collect::<Vec<_>>();

    if wide {
        table(
            &[
                "NAME",
                "STATUS",
                "CAPABILITIES",
                "PROVIDERS",
                "MODEL",
                "POLICY",
                "DESCRIPTION",
            ],
            &rows,
        )
    } else {
        table(&["NAME", "STATUS", "CAPABILITIES", "DESCRIPTION"], &rows)
    }
}

fn capability_table(capabilities: &[Capability], wide: bool) -> String {
    let rows = capabilities
        .iter()
        .map(|capability| {
            let mut row = vec![
                cell_text(&capability.metadata.name),
                capability.spec.effect.to_string(),
                capability.spec.provider.to_string(),
            ];
            if wide {
                row.extend([
                    capability.spec.risk.to_string(),
                    capability.spec.idempotency.to_string(),
                    capability.spec.permissions.len().to_string(),
                    capability
                        .status
                        .map_or_else(|| "Unknown".to_owned(), |status| status.to_string()),
                ]);
            }
            row.push(cell_text(&capability.spec.description));
            row
        })
        .collect::<Vec<_>>();

    if wide {
        table(
            &[
                "NAME",
                "EFFECT",
                "PROVIDER",
                "RISK",
                "IDEMPOTENCY",
                "PERMISSIONS",
                "STATUS",
                "DESCRIPTION",
            ],
            &rows,
        )
    } else {
        table(&["NAME", "EFFECT", "PROVIDER", "DESCRIPTION"], &rows)
    }
}

fn provider_table(providers: &[Provider], wide: bool) -> String {
    let rows = providers
        .iter()
        .map(|provider| {
            let mut row = vec![
                cell_text(&provider.metadata.name),
                provider
                    .status
                    .map_or_else(|| "Unknown".to_owned(), |status| status.to_string()),
                cell_text(&provider.spec.provider_type),
            ];
            if wide {
                row.push(cell_text(&provider.spec.credential_ref));
            }
            row.push(cell_text(&provider.spec.description));
            row
        })
        .collect::<Vec<_>>();

    if wide {
        table(
            &["NAME", "STATUS", "TYPE", "CREDENTIAL REF", "DESCRIPTION"],
            &rows,
        )
    } else {
        table(&["NAME", "STATUS", "TYPE", "DESCRIPTION"], &rows)
    }
}

fn agent_description(description: &AgentDescription) -> String {
    let agent = &description.agent;
    let mut lines = vec![
        format!("Name:         {}", cell_text(&agent.metadata.name)),
        format!("Status:       {}", effective_agent_status(agent)),
        format!("Enabled:      {}", agent.spec.enabled),
        format!("Description:  {}", cell_text(&agent.spec.description)),
        format!(
            "Model class:  {}",
            option_cell(agent.spec.model_class.as_deref())
        ),
        format!(
            "Policy:       {}",
            option_cell(agent.spec.policy_profile.as_deref())
        ),
        "Capabilities:".to_owned(),
    ];

    if description.capabilities.is_empty() {
        lines.push("  (none)".to_owned());
    } else {
        lines.extend(description.capabilities.iter().map(|capability| {
            format!(
                "  - {} [{}; provider={}]",
                capability.metadata.name, capability.spec.effect, capability.spec.provider
            )
        }));
    }

    lines.push("Providers:".to_owned());
    if description.providers.is_empty() {
        lines.push("  (none)".to_owned());
    } else {
        lines.extend(description.providers.iter().map(|provider| {
            format!(
                "  - {} [type={}]",
                provider.metadata.name,
                cell_text(&provider.spec.provider_type)
            )
        }));
    }

    lines.join("\n")
}

fn effective_agent_status(agent: &Agent) -> String {
    if !agent.spec.enabled {
        AgentStatus::Disabled.to_string()
    } else {
        agent.status.map_or_else(
            || AgentStatus::Pending.to_string(),
            |status| status.to_string(),
        )
    }
}

fn option_cell(value: Option<&str>) -> String {
    value.map_or_else(|| "-".to_owned(), cell_text)
}

fn cell_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn qualified_names<'a>(kind: &str, names: impl Iterator<Item = &'a str>) -> String {
    names
        .map(|name| format!("{kind}/{name}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn resource_name_agent(agent: &Agent) -> &str {
    &agent.metadata.name
}

fn resource_name_capability(capability: &Capability) -> &str {
    &capability.metadata.name
}

fn resource_name_provider(provider: &Provider) -> &str {
    &provider.metadata.name
}

fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths = headers
        .iter()
        .map(|header| header.chars().count())
        .collect::<Vec<_>>();
    for row in rows {
        for (index, cell) in row.iter().enumerate().take(widths.len()) {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(table_row(
        &headers.iter().map(ToString::to_string).collect::<Vec<_>>(),
        &widths,
    ));
    lines.extend(rows.iter().map(|row| table_row(row, &widths)));
    lines.join("\n")
}

fn table_row(cells: &[String], widths: &[usize]) -> String {
    let mut output = String::new();
    for (index, width) in widths.iter().copied().enumerate() {
        if index > 0 {
            output.push_str("   ");
        }
        let cell = cells.get(index).map_or("", String::as_str);
        output.push_str(cell);
        if index + 1 < widths.len() {
            output.push_str(&" ".repeat(width.saturating_sub(cell.chars().count())));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use crate::cli::OutputFormat;

    use super::{cell_text, table};

    #[test]
    fn table_has_no_trailing_whitespace() {
        let rendered = table(
            &["NAME", "STATUS"],
            &[vec!["reviewer".to_owned(), "Ready".to_owned()]],
        );
        assert!(rendered.lines().all(|line| !line.ends_with(' ')));
    }

    #[test]
    fn table_cells_remove_terminal_control_characters() {
        assert_eq!(cell_text("safe\n\u{1b}[31mtext"), "safe [31mtext");
    }

    #[test]
    fn output_format_values_remain_distinct() {
        assert_ne!(OutputFormat::Table, OutputFormat::Wide);
    }
}
