//! Small builders and temporary-file fixtures used by Dekopon workspace tests.
//!
//! Utilities belong here only when at least one other workspace crate uses them.

#![forbid(unsafe_code)]

use std::io::{self, Write};

use dekopon_capability::{EffectKind, Idempotency, Permission};
use dekopon_core::{AgentStatus, CapabilityId, ProviderId, RiskLevel};
use dekopon_protocol::{
    Agent, AgentSpec, ApiVersion, Capability, CapabilitySpec, Kind, ObjectMeta, Provider,
    ProviderSpec, ProviderStatus,
};
use tempfile::NamedTempFile;

/// Builder for concise agent fixtures.
#[derive(Clone, Debug)]
pub struct AgentBuilder {
    name: String,
    description: String,
    enabled: bool,
    instructions: Option<String>,
    capabilities: Vec<CapabilityId>,
    providers: Vec<ProviderId>,
    status: Option<AgentStatus>,
}

impl AgentBuilder {
    /// Starts an enabled agent fixture with no authority references.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: "Test agent".to_owned(),
            enabled: true,
            instructions: None,
            capabilities: Vec::new(),
            providers: Vec::new(),
            status: Some(AgentStatus::Ready),
        }
    }

    /// Sets the fixture description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the agent's standing orders, which are untrusted model text and grant nothing.
    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Adds a capability reference.
    #[must_use]
    pub fn capability(mut self, capability: CapabilityId) -> Self {
        self.capabilities.push(capability);
        self
    }

    /// Adds a provider reference.
    #[must_use]
    pub fn provider(mut self, provider: ProviderId) -> Self {
        self.providers.push(provider);
        self
    }

    /// Sets whether the agent is enabled.
    #[must_use]
    pub const fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Builds the protocol resource.
    #[must_use]
    pub fn build(self) -> Agent {
        Agent {
            api_version: ApiVersion::V1Alpha1,
            kind: Kind::Agent,
            metadata: ObjectMeta::named(self.name),
            spec: AgentSpec {
                description: self.description,
                enabled: self.enabled,
                instructions: self.instructions,
                capabilities: self.capabilities,
                providers: self.providers,
                model_class: None,
                policy_profile: None,
            },
            status: self.status,
        }
    }
}

/// Builder for concise capability fixtures.
#[derive(Clone, Debug)]
pub struct CapabilityBuilder {
    name: String,
    provider: ProviderId,
    effect: EffectKind,
}

impl CapabilityBuilder {
    /// Starts a low-risk, idempotent read capability fixture.
    #[must_use]
    pub fn new(name: impl Into<String>, provider: ProviderId) -> Self {
        Self {
            name: name.into(),
            provider,
            effect: EffectKind::ReadOnly,
        }
    }

    /// Sets the external-effect classification.
    #[must_use]
    pub const fn effect(mut self, effect: EffectKind) -> Self {
        self.effect = effect;
        self
    }

    /// Builds the protocol resource.
    #[must_use]
    pub fn build(self) -> Capability {
        let risk = match self.effect {
            EffectKind::ReadOnly => RiskLevel::Low,
            EffectKind::LocalWrite => RiskLevel::Medium,
            EffectKind::ExternalWrite => RiskLevel::High,
        };
        Capability {
            api_version: ApiVersion::V1Alpha1,
            kind: Kind::Capability,
            metadata: ObjectMeta::named(self.name),
            spec: CapabilitySpec {
                description: "Test capability".to_owned(),
                provider: self.provider,
                effect: self.effect,
                risk,
                idempotency: Idempotency::Idempotent,
                permissions: Vec::<Permission>::new(),
            },
            status: None,
        }
    }
}

/// Builder for provider fixtures.
#[derive(Clone, Debug)]
pub struct ProviderBuilder {
    name: String,
}

impl ProviderBuilder {
    /// Starts a provider fixture whose credential is a symbolic test reference.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// Builds the protocol resource.
    #[must_use]
    pub fn build(self) -> Provider {
        Provider {
            api_version: ApiVersion::V1Alpha1,
            kind: Kind::Provider,
            metadata: ObjectMeta::named(&self.name),
            spec: ProviderSpec {
                description: "Test provider".to_owned(),
                provider_type: self.name,
                credential_ref: "test-credential".to_owned(),
            },
            status: Some(ProviderStatus::Ready),
        }
    }
}

/// Writes configuration text to a temporary file that lives until the handle is dropped.
pub fn temporary_config(contents: &str) -> io::Result<NamedTempFile> {
    let mut file = NamedTempFile::new()?;
    file.write_all(contents.as_bytes())?;
    file.flush()?;
    Ok(file)
}
