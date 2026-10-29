use dekopon_broker_protocol::ChatTransportKind;
use dekopon_core::{PrincipalId, github::GithubEvent};
use dekopon_process::JobId;
use serde::{Serialize, Serializer};

use crate::wake::WakeId;

#[derive(Serialize)]
#[serde(
    tag = "trigger",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum WakeReasonView {
    Chat {
        transport_kind: ChatTransportKind,
        principal: Option<PrincipalId>,
        admission: Admission,
        steers: u32,
    },
    Webhook {
        source: WebhookSource,
        events: Vec<GithubEvent>,
    },
    Wake {
        #[serde(serialize_with = "serialize_wake_id")]
        wake: WakeId,
        scheduled_by: Option<PrincipalId>,
    },
    Job {
        #[serde(serialize_with = "serialize_job_id")]
        job: JobId,
    },
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Admission {
    New,
    FollowUp,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookSource {
    Github,
}

fn serialize_wake_id<S: Serializer>(id: &WakeId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&format_args!("w-{id}"))
}

fn serialize_job_id<S: Serializer>(id: &JobId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&format_args!("j-{id}"))
}

impl WakeReasonView {
    pub(crate) fn tool_result(&self, steers: u32) -> Result<String, serde_json::Error> {
        match self {
            Self::Chat {
                transport_kind,
                principal,
                admission,
                ..
            } => serde_json::to_string(&Self::Chat {
                transport_kind: *transport_kind,
                principal: principal.clone(),
                admission: *admission,
                steers,
            }),
            Self::Webhook { .. } | Self::Wake { .. } | Self::Job { .. } => {
                serde_json::to_string(self)
            }
        }
    }
}

/// Bounds the tool result since agent instructions can be arbitrarily large; an oversized view
/// returns a fixed diagnostic with none of the actual content, not a partial or unbounded copy.
pub const MAX_AGENT_CONFIG_TOOL_BYTES: usize = 128 * 1024;

/// Effect and risk are overwritten by the broker from the owner's constraint set, not the provider;
/// there is no input schema, since a model learns a provider from its own --help.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveCapabilityView {
    pub id: String,
    pub provider: String,
    pub description: String,
    pub effect: String,
    pub risk: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MemoryScopeView {
    PrivateConversation,
    SharedConversation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum MemoryConfigView {
    OneShot,
    Persistent {
        scope: MemoryScopeView,
        idle_timeout_ms: u64,
        max_turns: usize,
        max_bytes: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigView {
    pub max_steps: u32,
    pub max_capability_calls: u32,
    pub memory: MemoryConfigView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillView {
    pub name: String,
    pub description: String,
    pub resources: Vec<String>,
}

/// Deliberately has no field for a principal, subject, policy source or ID, execution constraint,
/// legacy credential map, model endpoint, chat token, or broker path, so an embedder cannot
/// accidentally populate one.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfigView {
    agent: AgentView,
    prompt: PromptView,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skills: Vec<SkillView>,
    session: SessionConfigView,
    effective_authorization: EffectiveAuthorizationView,
    security: SecurityView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentView {
    id: String,
    description: String,
    model_class: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptView {
    instructions: Option<String>,
    note: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct EffectiveAuthorizationView {
    engine: &'static str,
    view: &'static str,
    capabilities: Vec<EffectiveCapabilityView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct SecurityView {
    credentials_included: bool,
    raw_cedar_included: bool,
    identity_included: bool,
}

impl AgentConfigView {
    #[must_use]
    pub fn new(
        id: String,
        description: String,
        model_class: Option<String>,
        instructions: Option<String>,
        session: SessionConfigView,
        mut capabilities: Vec<EffectiveCapabilityView>,
    ) -> Self {
        capabilities.sort_by(|left, right| left.id.cmp(&right.id));
        Self {
            agent: AgentView {
                id,
                description,
                model_class,
            },
            prompt: PromptView {
                instructions,
                note: "A drn: name here is a name, not a secret.",
            },
            skills: Vec::new(),
            session,
            effective_authorization: EffectiveAuthorizationView {
                engine: "Cedar",
                view: "effective-grants",
                capabilities,
            },
            security: SecurityView {
                credentials_included: false,
                raw_cedar_included: false,
                identity_included: false,
            },
        }
    }

    #[must_use]
    pub fn with_skills(mut self, mut skills: Vec<SkillView>) -> Self {
        skills.sort_by(|left, right| left.name.cmp(&right.name));
        self.skills = skills;
        self
    }

    #[must_use]
    pub fn tool_result(&self) -> String {
        let encoded = match serde_json::to_string(self) {
            Ok(encoded) => encoded,
            Err(_) => {
                return r#"{"error":"agent configuration could not be serialized"}"#.to_owned();
            }
        };
        if encoded.len() > MAX_AGENT_CONFIG_TOOL_BYTES {
            return format!(
                "{{\"error\":\"agent configuration exceeds the safe tool-result bound\",\"maximumBytes\":{MAX_AGENT_CONFIG_TOOL_BYTES}}}"
            );
        }
        encoded
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{
        AgentConfigView, EffectiveCapabilityView, MAX_AGENT_CONFIG_TOOL_BYTES, MemoryConfigView,
        MemoryScopeView, SessionConfigView, SkillView,
    };

    fn view(instructions: String) -> AgentConfigView {
        AgentConfigView::new(
            "reviewer".to_owned(),
            "Reviews pull requests".to_owned(),
            Some("reasoning".to_owned()),
            Some(instructions),
            SessionConfigView {
                max_steps: 8,
                max_capability_calls: 16,
                memory: MemoryConfigView::Persistent {
                    scope: MemoryScopeView::PrivateConversation,
                    idle_timeout_ms: 900_000,
                    max_turns: 12,
                    max_bytes: 65_536,
                },
            },
            vec![EffectiveCapabilityView {
                id: "gh.pull-request.read".to_owned(),
                provider: "gh".to_owned(),
                description: "Reads one pull request".to_owned(),
                effect: "read-only".to_owned(),
                risk: "Low".to_owned(),
            }],
        )
    }

    #[test]
    fn wake_reason_ids_and_null_names_have_the_model_json_shape() {
        use super::{Admission, WakeReasonView};
        use crate::wake::WakeId;
        use dekopon_broker_protocol::ChatTransportKind;
        use dekopon_process::JobId;
        for (view, expected) in [
            (
                WakeReasonView::Chat {
                    transport_kind: ChatTransportKind::Local,
                    principal: None,
                    admission: Admission::New,
                    steers: 0,
                },
                serde_json::json!({"trigger":"chat","transportKind":"local","principal":null,"admission":"new","steers":3}),
            ),
            (
                WakeReasonView::Wake {
                    wake: WakeId(42),
                    scheduled_by: None,
                },
                serde_json::json!({"trigger":"wake","wake":"w-42","scheduledBy":null}),
            ),
            (
                WakeReasonView::Job { job: JobId::new(9) },
                serde_json::json!({"trigger":"job","job":"j-9"}),
            ),
        ] {
            let value: Value = serde_json::from_str(&view.tool_result(3).unwrap()).unwrap();
            assert_eq!(value, expected);
        }
    }

    #[test]
    fn view_contains_prompt_limits_and_effective_cedar_grants() {
        let encoded = view("Be concise.".to_owned()).tool_result();
        let value: Value = serde_json::from_str(&encoded).expect("view is JSON");

        assert_eq!(value["agent"]["id"], "reviewer");
        assert_eq!(value["prompt"]["instructions"], "Be concise.");
        assert_eq!(value["session"]["maxSteps"], 8);
        assert_eq!(
            value["session"]["memory"],
            serde_json::json!({
                "mode": "persistent",
                "scope": "privateConversation",
                "idle_timeout_ms": 900_000,
                "max_turns": 12,
                "max_bytes": 65_536
            })
        );
        assert_eq!(value["effectiveAuthorization"]["engine"], "Cedar");
        assert_eq!(
            value["effectiveAuthorization"]["capabilities"][0]["id"],
            "gh.pull-request.read"
        );
    }

    #[test]
    fn conversation_inspection_is_mode_only_for_one_shot_and_names_shared_scope() {
        assert_eq!(
            serde_json::to_value(MemoryConfigView::OneShot).expect("view serializes"),
            serde_json::json!({"mode": "oneShot"}),
            "persistent-only fields stay absent from one-shot inspection"
        );
        assert_eq!(
            serde_json::to_value(MemoryConfigView::Persistent {
                scope: MemoryScopeView::SharedConversation,
                idle_timeout_ms: 1,
                max_turns: 2,
                max_bytes: 3,
            })
            .expect("view serializes"),
            serde_json::json!({
                "mode": "persistent",
                "scope": "sharedConversation",
                "idle_timeout_ms": 1,
                "max_turns": 2,
                "max_bytes": 3
            })
        );
    }

    #[test]
    fn mounted_skills_are_listed_without_their_text() {
        let bare = view("Be concise.".to_owned()).tool_result();
        let value: Value = serde_json::from_str(&bare).expect("view is JSON");
        assert!(value.get("skills").is_none(), "{value}");

        let encoded = view("Be concise.".to_owned())
            .with_skills(vec![
                SkillView {
                    name: "release-notes".to_owned(),
                    description: "Use when drafting release notes.".to_owned(),
                    resources: Vec::new(),
                },
                SkillView {
                    name: "pull-request-review".to_owned(),
                    description: "Use when reviewing a pull request.".to_owned(),
                    resources: vec!["references/checklist.md".to_owned()],
                },
            ])
            .tool_result();
        let value: Value = serde_json::from_str(&encoded).expect("view is JSON");
        assert_eq!(value["skills"][0]["name"], "pull-request-review");
        assert_eq!(
            value["skills"][0]["resources"][0],
            "references/checklist.md"
        );
        assert_eq!(value["skills"][1]["name"], "release-notes");
        assert!(value["skills"][0].get("body").is_none());
    }

    #[test]
    fn view_structurally_omits_credentials_identity_and_raw_policy() {
        let encoded = view("No secrets live here.".to_owned()).tool_result();
        let value: Value = serde_json::from_str(&encoded).expect("view is JSON");

        assert_eq!(value["security"]["credentialsIncluded"], false);
        assert_eq!(value["security"]["rawCedarIncluded"], false);
        assert_eq!(value["security"]["identityIncluded"], false);
        assert!(value.get("principal").is_none());
        assert!(value.get("subject").is_none());
        assert!(value.get("credential").is_none());
        assert!(value.get("modelEndpoint").is_none());
    }

    #[test]
    fn oversized_view_returns_no_partial_configuration() {
        let sentinel = "do-not-copy-this-tail";
        let instructions = format!("{}{sentinel}", "x".repeat(MAX_AGENT_CONFIG_TOOL_BYTES));
        let encoded = view(instructions).tool_result();
        let value: Value = serde_json::from_str(&encoded).expect("diagnostic is JSON");

        assert_eq!(
            value["error"],
            "agent configuration exceeds the safe tool-result bound"
        );
        assert_eq!(value["maximumBytes"], MAX_AGENT_CONFIG_TOOL_BYTES);
        assert!(!encoded.contains(sentinel));
        assert!(value.get("agent").is_none());
    }
}
