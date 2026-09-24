//! Authored resources reject unknown fields so a misspelled security-relevant setting fails to load
//! instead of being silently ignored.

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
use std::{collections::BTreeMap, fmt, path::PathBuf};

pub use dekopon_core::AgentStatus;
use dekopon_core::{CapabilityId, ProviderId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ApiVersion {
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

/// Kind carries its own type rather than a shared enum so serde rejects a document naming another
/// resource on decode, even for callers that skip the configuration loader.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "PascalCase")]
pub enum AgentKind {
    Agent,
}

impl fmt::Display for AgentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ObjectMeta {
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl ObjectMeta {
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            labels: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AgentSpec {
    pub description: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Instructions are untrusted model-shaping text only; they can never assert identity, name a
    /// principal, widen a capability, or influence authorization, since broker policy never reads
    /// this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Skill files are untrusted model-reference text exactly like instructions: read on demand,
    /// and they grant no authority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<PathBuf>,
    /// Capabilities the agent may propose. This list itself grants no provider authority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityId>,
    /// Catalog validation requires this list to exactly match the providers the agent's
    /// capabilities route to and refuses any drift between them; the list itself grants nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<ProviderId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_class: Option<String>,
    /// This field is inert; no runtime authority reader consumes it, since broker authority comes
    /// only from the Cedar policy file and the per-capability constraint sets in broker.yaml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_profile: Option<String>,
}

const fn default_enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Agent {
    pub api_version: ApiVersion,
    pub kind: AgentKind,
    pub metadata: ObjectMeta,
    pub spec: AgentSpec,
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
}
