//! Versioned, transport-independent Dekopon resources.
//!
//! The `v1alpha1` shape is inspired by Kubernetes resource documents: each authored
//! resource carries an API version, kind, metadata, spec, and an optional observed status
//! where useful. It is intentionally smaller than the Kubernetes API machinery.
//!
//! Authored structures reject unknown fields. This catches misspelled security-relevant
//! settings today; a future API version can introduce an explicit compatibility strategy
//! if network negotiation requires one.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, fmt};

use dekopon_capability::{EffectKind, Idempotency, Permission};
pub use dekopon_core::AgentStatus;
use dekopon_core::{CapabilityId, ProviderId, RiskLevel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// API version supported by this crate.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
pub enum ApiVersion {
    /// Initial alpha resource format.
    #[serde(rename = "dekopon.dev/v1alpha1")]
    V1Alpha1,
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V1Alpha1 => formatter.write_str("dekopon.dev/v1alpha1"),
        }
    }
}

/// Resource kind discriminator.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "PascalCase")]
pub enum Kind {
    /// An agent resource.
    Agent,
    /// A capability resource.
    Capability,
    /// A provider resource.
    Provider,
    /// A list of agents.
    AgentList,
    /// A list of capabilities.
    CapabilityList,
    /// A list of providers.
    ProviderList,
}

impl fmt::Display for Kind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

/// Common authored metadata.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ObjectMeta {
    /// Resource name. The configuration loader validates it as the kind-specific ID type.
    pub name: String,
    /// Operator-defined labels with stable ordering.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl ObjectMeta {
    /// Creates metadata without labels.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            labels: BTreeMap::new(),
        }
    }
}

/// Desired state of an agent.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentSpec {
    /// Concise operator-facing purpose.
    pub description: String,
    /// Whether orchestration may schedule the agent.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Capabilities the agent may propose. This list itself grants no provider authority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityId>,
    /// Providers the agent is expected to use through its capabilities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderId>,
    /// Optional model class selected by future orchestration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_class: Option<String>,
    /// Optional declarative policy profile name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_profile: Option<String>,
}

const fn default_enabled() -> bool {
    true
}

/// A declarative agent resource.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Agent {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Must be [`Kind::Agent`].
    pub kind: Kind,
    /// Resource identity and labels.
    pub metadata: ObjectMeta,
    /// Desired agent state.
    pub spec: AgentSpec,
    /// Optional observed state. Local configuration may provide it for operator workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentStatus>,
}

/// Desired state of a capability.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CapabilitySpec {
    /// Concise operator-facing purpose.
    pub description: String,
    /// Provider expected to implement this capability.
    pub provider: ProviderId,
    /// External-effect classification.
    pub effect: EffectKind,
    /// Coarse risk classification available to policy.
    pub risk: RiskLevel,
    /// Declared retry behavior.
    pub idempotency: Idempotency,
    /// Least-privilege provider permissions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<Permission>,
}

/// Availability reported for a capability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum CapabilityStatus {
    /// The declared provider is available.
    Available,
    /// The declared provider is unavailable.
    Unavailable,
    /// Availability has not been observed.
    Unknown,
}

impl fmt::Display for CapabilityStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

/// A declarative capability resource.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Capability {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Must be [`Kind::Capability`].
    pub kind: Kind,
    /// Resource identity and labels.
    pub metadata: ObjectMeta,
    /// Desired capability state.
    pub spec: CapabilitySpec,
    /// Optional observed state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<CapabilityStatus>,
}

/// Desired state of a provider connection.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderSpec {
    /// Concise operator-facing purpose.
    pub description: String,
    /// Provider implementation family, such as `github`.
    #[serde(rename = "type")]
    pub provider_type: String,
    /// Symbolic credential reference resolved only by a future broker.
    pub credential_ref: String,
}

/// Availability reported for a provider.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum ProviderStatus {
    /// The provider declaration is ready for use.
    Ready,
    /// The provider is not available.
    Unavailable,
    /// Availability has not been observed.
    Unknown,
}

impl fmt::Display for ProviderStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

/// A declarative provider resource.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Provider {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Must be [`Kind::Provider`].
    pub kind: Kind,
    /// Resource identity and labels.
    pub metadata: ObjectMeta,
    /// Desired provider state.
    pub spec: ProviderSpec,
    /// Optional observed state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProviderStatus>,
}

/// Versioned agent-list response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentList {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Must be [`Kind::AgentList`].
    pub kind: Kind,
    /// Agents in deterministic name order.
    pub items: Vec<Agent>,
}

impl AgentList {
    /// Creates a `v1alpha1` agent list.
    #[must_use]
    pub const fn new(items: Vec<Agent>) -> Self {
        Self {
            api_version: ApiVersion::V1Alpha1,
            kind: Kind::AgentList,
            items,
        }
    }
}

/// Versioned capability-list response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CapabilityList {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Must be [`Kind::CapabilityList`].
    pub kind: Kind,
    /// Capabilities in deterministic name order.
    pub items: Vec<Capability>,
}

impl CapabilityList {
    /// Creates a `v1alpha1` capability list.
    #[must_use]
    pub const fn new(items: Vec<Capability>) -> Self {
        Self {
            api_version: ApiVersion::V1Alpha1,
            kind: Kind::CapabilityList,
            items,
        }
    }
}

/// Versioned provider-list response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderList {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Must be [`Kind::ProviderList`].
    pub kind: Kind,
    /// Providers in deterministic name order.
    pub items: Vec<Provider>,
}

impl ProviderList {
    /// Creates a `v1alpha1` provider list.
    #[must_use]
    pub const fn new(items: Vec<Provider>) -> Self {
        Self {
            api_version: ApiVersion::V1Alpha1,
            kind: Kind::ProviderList,
            items,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dekopon_capability::{EffectKind, Idempotency};
    use dekopon_core::{AgentStatus, ProviderId, RiskLevel};
    use schemars::schema_for;

    use super::{Agent, AgentSpec, ApiVersion, Capability, CapabilitySpec, Kind, ObjectMeta};

    fn agent() -> Agent {
        Agent {
            api_version: ApiVersion::V1Alpha1,
            kind: Kind::Agent,
            metadata: ObjectMeta {
                name: "reviewer".to_owned(),
                labels: BTreeMap::from([("team".to_owned(), "platform".to_owned())]),
            },
            spec: AgentSpec {
                description: "Reviews pull requests".to_owned(),
                enabled: true,
                capabilities: vec![
                    "github.pull-request.read"
                        .parse()
                        .expect("valid capability fixture"),
                ],
                providers: vec!["github".parse().expect("valid provider fixture")],
                model_class: Some("reasoning".to_owned()),
                policy_profile: Some("review-read-only".to_owned()),
            },
            status: Some(AgentStatus::Ready),
        }
    }

    #[test]
    fn agent_round_trips_through_json_and_yaml() {
        let original = agent();

        let json = serde_json::to_string(&original).expect("agent serializes as JSON");
        let from_json = serde_json::from_str::<Agent>(&json).expect("agent parses as JSON");
        assert_eq!(from_json, original);

        let yaml = serde_yaml::to_string(&original).expect("agent serializes as YAML");
        let from_yaml = serde_yaml::from_str::<Agent>(&yaml).expect("agent parses as YAML");
        assert_eq!(from_yaml, original);
        assert!(yaml.contains("apiVersion: dekopon.dev/v1alpha1"));
    }

    #[test]
    fn rejects_unknown_authored_fields() {
        let input = r#"
apiVersion: dekopon.dev/v1alpha1
kind: Capability
metadata:
  name: github.pull-request.read
spec:
  description: Reads pull requests
  provider: github
  effect: read-only
  risk: Low
  idempotency: idempotent
  permisssions: []
"#;
        let error = serde_yaml::from_str::<Capability>(input)
            .expect_err("misspelled permissions must not be ignored");
        assert!(error.to_string().contains("unknown field `permisssions`"));
    }

    #[test]
    fn generates_json_schema() {
        let schema = schema_for!(Agent);
        let encoded = serde_json::to_value(schema).expect("schema serializes");
        assert_eq!(encoded["title"], "Agent");
    }

    #[test]
    fn capability_wire_values_are_explicit() {
        let capability = Capability {
            api_version: ApiVersion::V1Alpha1,
            kind: Kind::Capability,
            metadata: ObjectMeta::named("github.pull-request.read"),
            spec: CapabilitySpec {
                description: "Reads pull requests".to_owned(),
                provider: "github".parse::<ProviderId>().expect("valid fixture"),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                idempotency: Idempotency::Idempotent,
                permissions: Vec::new(),
            },
            status: None,
        };
        let value = serde_json::to_value(capability).expect("capability serializes");

        assert_eq!(value["spec"]["effect"], "read-only");
        assert_eq!(value["spec"]["risk"], "Low");
    }
}
