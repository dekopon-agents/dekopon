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

use std::{collections::BTreeMap, fmt, path::PathBuf};

pub use dekopon_core::AgentStatus;
use dekopon_core::{CapabilityId, ProviderId};
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

/// Kind discriminator accepted for an [`Agent`] document.
///
/// Single-variant: any other `kind` fails to decode. Carrying the discriminator in the type
/// rather than in a shared enum is what makes `serde` refuse a document naming another resource
/// while decoding it here, without a caller passing through the configuration loader first.
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum AgentKind {
    /// An agent resource.
    Agent,
}

impl fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

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
    /// Skill directories mounted into the agent's sessions, each holding a `SKILL.md`.
    ///
    /// A relative path resolves against the catalog file's own directory. The loader reads every
    /// skill at catalog load and refuses the catalog when one cannot be read, so a routed agent
    /// never starts with a skill it cannot show. Skill text is untrusted model text exactly as
    /// `instructions` is: it is reference material the model reads on demand, and it grants
    /// nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<PathBuf>,
    /// Capabilities the agent may propose. This list itself grants no provider authority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityId>,
    /// Providers the agent is expected to use through its capabilities.
    ///
    /// Catalog validation holds this to exactly that: every provider the agent's declared
    /// capabilities route to must appear here, and a provider listed here must be reachable
    /// through one of them. The list grants nothing; catalog validation refuses drift
    /// from the declared capabilities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderId>,
    /// Model class `dekopond` resolves against its configured models.
    ///
    /// Gateway resolution (`crates/dekopond/src/routes.rs`, `RoutingTable::bind`) requires this
    /// for a routed agent only when the route has no explicit model. It selects the first
    /// configured model offering the class; no matching model fails startup. An explicit
    /// route model overrides it, and an unrouted agent needs no model class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_class: Option<String>,
    /// Reserved declarative policy profile name, consumed by no shipped component.
    ///
    /// Authored catalog metadata; no runtime authority reader consumes it. Broker
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dekopon_core::AgentStatus;

    use super::{Agent, AgentKind, AgentSpec, ApiVersion, ObjectMeta};

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
                skills: vec!["skills/pull-request-review".into()],
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
        assert!(yaml.contains("skills:"), "{yaml}");
    }

    /// An agent that mounts nothing serializes without the key, exactly as `instructions` does.
    #[test]
    fn absent_skills_stay_absent_through_a_round_trip() {
        let mut original = agent();
        original.spec.skills = Vec::new();

        let value = serde_json::to_value(&original).expect("agent serializes");
        assert!(value["spec"].get("skills").is_none(), "{value}");
        let yaml = serde_yaml::to_string(&original).expect("agent serializes as YAML");
        assert!(!yaml.contains("skills"), "{yaml}");
        let decoded = serde_yaml::from_str::<Agent>(&yaml).expect("agent parses as YAML");
        assert_eq!(decoded, original);
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
    /// External consumers of this published crate decode this type directly and never pass
    /// through the configuration loader's header check, so the kind discriminator has to be
    /// part of the type.
    #[test]
    fn rejects_a_document_whose_kind_names_another_resource() {
        let input = r#"
apiVersion: dekopon.dev/v1alpha1
kind: Secret
metadata:
  name: reviewer
spec:
  description: Reviews pull requests
"#;
        let error = serde_yaml::from_str::<Agent>(input)
            .expect_err("an agent document must not decode with another resource's kind");
        assert!(error.to_string().contains("Secret"), "{error}");
    }

    #[test]
    fn rejects_unknown_authored_fields() {
        let input = r#"
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: Reviews pull requests
  capabilties: []
"#;
        let error = serde_yaml::from_str::<Agent>(input)
            .expect_err("misspelled capabilities must not be ignored");
        assert!(error.to_string().contains("unknown field `capabilties`"));
    }

    #[cfg(feature = "schemars")]
    #[test]
    fn generates_json_schema() {
        let schema = schemars::schema_for!(Agent);
        let encoded = serde_json::to_value(schema).expect("schema serializes");
        assert_eq!(encoded["title"], "Agent");
    }
}
