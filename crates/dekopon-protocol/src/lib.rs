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
use serde::{Deserialize, Serialize};

/// API version supported by this crate.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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

/// Declares one single-variant kind discriminator for one resource type.
///
/// Each authored resource carries its own kind type rather than the shared [`Kind`], so
/// `serde` refuses a document whose `kind` names a different resource while decoding it here.
/// The shared enum stays reachable through [`From`] for callers that report kinds generically.
macro_rules! resource_kind {
    ($name:ident, $variant:ident, $summary:literal, $variant_doc:literal) => {
        #[doc = $summary]
        ///
        /// Single-variant: any other `kind` fails to decode.
        #[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[serde(rename_all = "PascalCase")]
        pub enum $name {
            #[doc = $variant_doc]
            $variant,
        }

        impl From<$name> for Kind {
            fn from(_: $name) -> Self {
                Self::$variant
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&Kind::from(*self), formatter)
            }
        }
    };
}

resource_kind!(
    AgentKind,
    Agent,
    "Kind discriminator accepted for an [`Agent`] document.",
    "An agent resource."
);
resource_kind!(
    CapabilityKind,
    Capability,
    "Kind discriminator accepted for a [`Capability`] document.",
    "A capability resource."
);
resource_kind!(
    ProviderKind,
    Provider,
    "Kind discriminator accepted for a [`Provider`] document.",
    "A provider resource."
);
resource_kind!(
    AgentListKind,
    AgentList,
    "Kind discriminator accepted for an [`AgentList`] document.",
    "A list of agents."
);
resource_kind!(
    CapabilityListKind,
    CapabilityList,
    "Kind discriminator accepted for a [`CapabilityList`] document.",
    "A list of capabilities."
);
resource_kind!(
    ProviderListKind,
    ProviderList,
    "Kind discriminator accepted for a [`ProviderList`] document.",
    "A list of providers."
);

/// Common authored metadata.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentSpec {
    /// Concise operator-facing purpose.
    pub description: String,
    /// Whether orchestration may schedule the agent.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// The agent's standing orders, handed to the model as its system prompt.
    ///
    /// This is untrusted model text by definition. It shapes how an agent answers and nothing
    /// else: it can never assert identity or authority, name a principal, widen a capability, or
    /// influence an authorization decision. Everything an agent may actually do comes from broker
    /// policy, which never reads this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Capabilities the agent may propose. This list itself grants no provider authority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityId>,
    /// Providers the agent is expected to use through its capabilities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderId>,
    /// Model class `dekopond` resolves against its configured models.
    ///
    /// Required for any agent a gateway route references: `dekopond` fails at startup when a
    /// routed agent leaves it unset, and the value decides which model receives the agent's
    /// instructions. It is optional here only because an agent the gateway never routes — one read
    /// by the CLI alone — does not need one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_class: Option<String>,
    /// Reserved declarative policy profile name, consumed by no shipped component.
    ///
    /// Authored and rendered by `dekopon get`/`describe`, and nothing else reads it. Broker
    /// authority comes from the owner-authored Cedar policy file and per-capability constraint
    /// sets in `broker.yaml`; naming a profile here selects no policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_profile: Option<String>,
}

const fn default_enabled() -> bool {
    true
}

/// A declarative agent resource.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Agent {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Fixed `Agent` discriminator; any other kind fails to decode.
    pub kind: AgentKind,
    /// Resource identity and labels.
    pub metadata: ObjectMeta,
    /// Desired agent state.
    pub spec: AgentSpec,
    /// Optional observed state. Local configuration may provide it for operator workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentStatus>,
}

/// Desired state of a capability.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

/// Availability authored for a capability.
///
/// Nothing in Dekopon observes provider availability, so every value here came from the catalog
/// file the CLI is echoing back.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Capability {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Fixed `Capability` discriminator; any other kind fails to decode.
    pub kind: CapabilityKind,
    /// Resource identity and labels.
    pub metadata: ObjectMeta,
    /// Desired capability state.
    pub spec: CapabilitySpec,
    /// Optional status. Authored, not observed: no component populates or refreshes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<CapabilityStatus>,
}

/// Desired state of a provider connection.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderSpec {
    /// Concise operator-facing purpose.
    pub description: String,
    /// Provider implementation family, such as `github`.
    #[serde(rename = "type")]
    pub provider_type: String,
    /// Reserved symbolic credential reference, consumed by no shipped component.
    ///
    /// Authored and rendered by `dekopon get`/`describe`, and nothing else reads it. Credential
    /// binding is owned by the broker's per-capability constraint sets and its `0600` credentials
    /// file, neither of which consults the catalog.
    pub credential_ref: String,
}

/// Availability authored for a provider.
///
/// Nothing in Dekopon observes provider availability, so every value here came from the catalog
/// file the CLI is echoing back.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Provider {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Fixed `Provider` discriminator; any other kind fails to decode.
    pub kind: ProviderKind,
    /// Resource identity and labels.
    pub metadata: ObjectMeta,
    /// Desired provider state.
    pub spec: ProviderSpec,
    /// Optional status. Authored, not observed: no component populates or refreshes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProviderStatus>,
}

/// Versioned agent-list response.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentList {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Fixed `AgentList` discriminator; any other kind fails to decode.
    pub kind: AgentListKind,
    /// Agents in deterministic name order.
    pub items: Vec<Agent>,
}

impl AgentList {
    /// Creates a `v1alpha1` agent list.
    #[must_use]
    pub const fn new(items: Vec<Agent>) -> Self {
        Self {
            api_version: ApiVersion::V1Alpha1,
            kind: AgentListKind::AgentList,
            items,
        }
    }
}

/// Versioned capability-list response.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CapabilityList {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Fixed `CapabilityList` discriminator; any other kind fails to decode.
    pub kind: CapabilityListKind,
    /// Capabilities in deterministic name order.
    pub items: Vec<Capability>,
}

impl CapabilityList {
    /// Creates a `v1alpha1` capability list.
    #[must_use]
    pub const fn new(items: Vec<Capability>) -> Self {
        Self {
            api_version: ApiVersion::V1Alpha1,
            kind: CapabilityListKind::CapabilityList,
            items,
        }
    }
}

/// Versioned provider-list response.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProviderList {
    /// Resource schema version.
    pub api_version: ApiVersion,
    /// Fixed `ProviderList` discriminator; any other kind fails to decode.
    pub kind: ProviderListKind,
    /// Providers in deterministic name order.
    pub items: Vec<Provider>,
}

impl ProviderList {
    /// Creates a `v1alpha1` provider list.
    #[must_use]
    pub const fn new(items: Vec<Provider>) -> Self {
        Self {
            api_version: ApiVersion::V1Alpha1,
            kind: ProviderListKind::ProviderList,
            items,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dekopon_capability::{EffectKind, Idempotency};
    use dekopon_core::{AgentStatus, ProviderId, RiskLevel};

    use super::{
        Agent, AgentKind, AgentSpec, ApiVersion, Capability, CapabilityKind, CapabilitySpec,
        ObjectMeta,
    };

    fn agent() -> Agent {
        Agent {
            api_version: ApiVersion::V1Alpha1,
            kind: AgentKind::Agent,
            metadata: ObjectMeta {
                name: "reviewer".to_owned(),
                labels: BTreeMap::from([("team".to_owned(), "platform".to_owned())]),
            },
            spec: AgentSpec {
                description: "Reviews pull requests".to_owned(),
                enabled: true,
                instructions: Some("Review the diff and comment; never approve.".to_owned()),
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
        assert!(yaml.contains("instructions:"));
    }

    /// Standing orders are optional and absent rather than empty when unauthored.
    ///
    /// An agent with no `instructions` must serialize without the key at all, so a round trip
    /// through the catalog cannot turn "the operator wrote none" into an empty system prompt.
    #[test]
    fn absent_instructions_stay_absent_through_a_round_trip() {
        let mut original = agent();
        original.spec.instructions = None;

        let value = serde_json::to_value(&original).expect("agent serializes");
        assert!(value["spec"].get("instructions").is_none(), "{value}");

        let yaml = serde_yaml::to_string(&original).expect("agent serializes as YAML");
        assert!(!yaml.contains("instructions"), "{yaml}");
        let decoded = serde_yaml::from_str::<Agent>(&yaml).expect("agent parses as YAML");
        assert_eq!(decoded, original);
        assert!(decoded.spec.instructions.is_none());
    }

    /// A mismatched `kind` must fail here, not only in `dekopon-config`.
    ///
    /// External consumers of this published crate decode these types directly and never pass
    /// through the configuration loader's header check, so the kind discriminator has to be
    /// part of the type.
    #[test]
    fn rejects_a_document_whose_kind_names_another_resource() {
        let input = r#"
apiVersion: dekopon.dev/v1alpha1
kind: ProviderList
metadata:
  name: reviewer
spec:
  description: Reviews pull requests
"#;
        let error = serde_yaml::from_str::<Agent>(input)
            .expect_err("an agent document must not decode with another resource's kind");
        assert!(error.to_string().contains("ProviderList"), "{error}");

        let capability = r#"
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: github.pull-request.read
spec:
  description: Reads pull requests
  provider: github
  effect: read-only
  risk: Low
  idempotency: idempotent
"#;
        let error = serde_yaml::from_str::<Capability>(capability)
            .expect_err("a capability document must not decode with another resource's kind");
        assert!(error.to_string().contains("Agent"), "{error}");
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

    #[cfg(feature = "schemars")]
    #[test]
    fn generates_json_schema() {
        let schema = schemars::schema_for!(Agent);
        let encoded = serde_json::to_value(schema).expect("schema serializes");
        assert_eq!(encoded["title"], "Agent");
    }

    #[test]
    fn capability_wire_values_are_explicit() {
        let capability = Capability {
            api_version: ApiVersion::V1Alpha1,
            kind: CapabilityKind::Capability,
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
