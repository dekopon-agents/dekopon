//! Read abstraction used by command execution.

use dekopon_config::{CatalogSnapshot, LocalCatalog};
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

/// Read-only resource interface shared by local config and a future daemon client.
pub trait ResourceReader {
    /// Lists all agents in deterministic order.
    fn list_agents(&self) -> Result<Vec<Agent>, CatalogError>;
    /// Gets one agent by name.
    fn get_agent(&self, name: &AgentId) -> Result<Agent, CatalogError>;
    /// Lists all capabilities in deterministic order.
    fn list_capabilities(&self) -> Result<Vec<Capability>, CatalogError>;
    /// Gets one capability by name.
    fn get_capability(&self, name: &CapabilityId) -> Result<Capability, CatalogError>;
    /// Lists all providers in deterministic order.
    fn list_providers(&self) -> Result<Vec<Provider>, CatalogError>;
    /// Gets one provider by name.
    fn get_provider(&self, name: &ProviderId) -> Result<Provider, CatalogError>;
}

/// [`ResourceReader`] backed by one parsed local catalog.
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
}

impl ResourceReader for LocalConfigReader {
    fn list_agents(&self) -> Result<Vec<Agent>, CatalogError> {
        Ok(self.catalog.agents().cloned().collect())
    }

    fn get_agent(&self, name: &AgentId) -> Result<Agent, CatalogError> {
        self.catalog
            .agent(name)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound {
                kind: "agent",
                name: name.to_string(),
            })
    }

    fn list_capabilities(&self) -> Result<Vec<Capability>, CatalogError> {
        Ok(self.catalog.capabilities().cloned().collect())
    }

    fn get_capability(&self, name: &CapabilityId) -> Result<Capability, CatalogError> {
        self.catalog
            .capability(name)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound {
                kind: "capability",
                name: name.to_string(),
            })
    }

    fn list_providers(&self) -> Result<Vec<Provider>, CatalogError> {
        Ok(self.catalog.providers().cloned().collect())
    }

    fn get_provider(&self, name: &ProviderId) -> Result<Provider, CatalogError> {
        self.catalog
            .provider(name)
            .cloned()
            .ok_or_else(|| CatalogError::NotFound {
                kind: "provider",
                name: name.to_string(),
            })
    }
}
