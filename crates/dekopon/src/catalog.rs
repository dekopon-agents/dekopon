//! Read abstraction used by command execution.

use dekopon_config::{CatalogSnapshot, LocalCatalog, Skill};
use dekopon_core::{AgentId, CapabilityId, ProviderId};
use dekopon_protocol::{Agent, Capability, Provider};
use thiserror::Error;

/// Resource lookup failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CatalogError {
    /// A requested resource does not exist in the validated catalog.
    #[error("{kind} {name:?} not found")]
    NotFound {
        /// Singular lower-case resource kind.
        kind: &'static str,
        /// Requested identifier.
        name: String,
    },
}

/// Read-only resource access backed by one parsed local catalog.
#[derive(Clone, Debug)]
pub struct LocalConfigReader {
    catalog: LocalCatalog,
}

impl LocalConfigReader {
    /// Wraps an already validated catalog.
    #[must_use]
    pub const fn new(catalog: LocalCatalog) -> Self {
        Self { catalog }
    }

    /// Returns a canonical owned view of the loaded configuration.
    #[must_use]
    pub fn snapshot(&self) -> CatalogSnapshot {
        self.catalog.snapshot()
    }

    /// Returns the loaded source path for validation output.
    #[must_use]
    pub fn source_display(&self) -> String {
        self.catalog.source().display().to_string()
    }

    /// Lists all agents in deterministic order.
    #[must_use]
    pub fn list_agents(&self) -> Vec<Agent> {
        self.catalog.agents().cloned().collect()
    }

    /// Gets one agent by name.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the validated catalog declares no such agent.
    pub fn get_agent(&self, name: &AgentId) -> Result<Agent, CatalogError> {
        self.catalog
            .agent(name)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound {
                kind: "agent",
                name: name.to_string(),
            })
    }

    /// The skills one agent mounts, in the order its `spec.skills` names them.
    #[must_use]
    pub fn agent_skills(&self, name: &AgentId) -> Vec<Skill> {
        self.catalog.agent_skills(name).to_vec()
    }

    /// Lists all capabilities in deterministic order.
    #[must_use]
    pub fn list_capabilities(&self) -> Vec<Capability> {
        self.catalog.capabilities().cloned().collect()
    }

    /// Gets one capability by name.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the validated catalog declares no such capability.
    pub fn get_capability(&self, name: &CapabilityId) -> Result<Capability, CatalogError> {
        self.catalog
            .capability(name)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound {
                kind: "capability",
                name: name.to_string(),
            })
    }

    /// Lists all providers in deterministic order.
    #[must_use]
    pub fn list_providers(&self) -> Vec<Provider> {
        self.catalog.providers().cloned().collect()
    }

    /// Gets one provider by name.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the validated catalog declares no such provider.
    pub fn get_provider(&self, name: &ProviderId) -> Result<Provider, CatalogError> {
        self.catalog
            .provider(name)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound {
                kind: "provider",
                name: name.to_string(),
            })
    }
}
