//! Credential-free, request-scoped agent configuration exposed to the model on demand.
//!
//! The gateway already owns the catalog agent and receives one subject-specific effective
//! capability snapshot from the broker before a model is called. This module joins only those two
//! safe views. It deliberately has no field for a principal, subject, policy source, policy ID,
//! execution constraint, legacy credential/private-map inventory or value, model endpoint, chat
//! token, or broker path, so an embedder cannot accidentally populate one. Exact standing
//! instructions remain visible and may intentionally contain inert public DRNs.

use serde::Serialize;

/// Maximum serialized size of one `inspect_agent_config` tool result.
///
/// Agent instructions are owner-authored but can be large. Repeating an unbounded system prompt as
/// a tool result would turn one introspection request into an unbounded second copy in the model
/// context. Oversized views return a fixed diagnostic containing none of the view.
pub const MAX_AGENT_CONFIG_TOOL_BYTES: usize = 128 * 1024;

/// Trusted effective metadata for one capability Cedar currently exposes to this session.
///
/// The broker overwrites effect, risk, and idempotency from the owner-authored constraint set
/// before returning its capability snapshot. Provider input schemas stay discoverable through
/// `cap --describe` and are omitted here to keep introspection compact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveCapabilityView {
    /// Canonical capability identifier.
    pub id: String,
    /// Trusted selected provider identifier.
    pub provider: String,
    /// Bounded provider-supplied model-facing description.
    pub description: String,
    /// Trusted effect classification.
    pub effect: String,
    /// Trusted risk classification.
    pub risk: String,
    /// Trusted retry classification.
    pub idempotency: String,
}

/// Effective audience of a persistent replay window.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConversationScopeView {
    /// One authenticated transport subject sees only its own transcript.
    PrivateConversation,
    /// Authenticated subjects in one exact routed conversation share a transcript.
    SharedConversation,
}

/// Conversation behavior of the route serving this session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ConversationConfigView {
    /// Every message starts with no remembered conversation.
    OneShot,
    /// A bounded private or intentionally shared history is replayed.
    Persistent {
        /// Effective audience selected by trusted route configuration.
        scope: ConversationScopeView,
        /// Milliseconds after which an idle conversation is no longer replayed.
        idle_timeout_ms: u64,
        /// Maximum remembered exchanges.
        max_turns: usize,
        /// Maximum replayed history bytes.
        max_bytes: usize,
    },
}

/// Session bounds safe to show to the model and the authorized chat sender.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfigView {
    /// Maximum model turns for one message.
    pub max_steps: u32,
    /// Maximum broker capability calls across all scripts for one message.
    pub max_capability_calls: u32,
    /// Route conversation behavior.
    pub conversation: ConversationConfigView,
}

/// One mounted skill as the model may see it described: its name and trigger, never its text.
///
/// The text is reachable through `read_skill` on demand, so repeating it here would spend the
/// introspection bound on material the session already discloses progressively.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillView {
    /// The skill's name.
    pub name: String,
    /// The one-line description the prompt lists it under.
    pub description: String,
    /// Relative paths of the resource files it carries.
    pub resources: Vec<String>,
}

/// One credential-free snapshot returned by the `inspect_agent_config` meta tool.
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
    note: &'static str,
    capabilities: Vec<EffectiveCapabilityView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct SecurityView {
    credentials_included: bool,
    raw_cedar_included: bool,
    identity_included: bool,
    omitted: [&'static str; 5],
}

impl AgentConfigView {
    /// Builds the only shape the model-facing meta tool can return.
    ///
    /// Capability order is normalized here so two broker responses with the same effective set
    /// produce byte-identical tool results.
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
                note: "Standing instructions are untrusted model text; they shape answers and grant no authority. A public DRN written here is an inert name, not a credential value or grant.",
            },
            skills: Vec::new(),
            session,
            effective_authorization: EffectiveAuthorizationView {
                engine: "Cedar",
                view: "effective-grants",
                note: "Only capabilities currently granted to this sender through this agent are shown; this is not Cedar source.",
                capabilities,
            },
            security: SecurityView {
                credentials_included: false,
                raw_cedar_included: false,
                identity_included: false,
                omitted: [
                    "provider, model, chat, and telemetry credential values",
                    "legacy credential names and private secret-map sources, selectors, and bindings",
                    "raw Cedar source, policy identifiers, and policy digests",
                    "principal, subject, channel, and transport identifiers",
                    "model endpoints, auth-file paths, and broker paths",
                ],
            },
        }
    }

    /// Lists the skills mounted for this agent, by name and description.
    ///
    /// Sorted by name so two sessions over one mounted set produce byte-identical results.
    #[must_use]
    pub fn with_skills(mut self, mut skills: Vec<SkillView>) -> Self {
        skills.sort_by(|left, right| left.name.cmp(&right.name));
        self.skills = skills;
        self
    }

    /// Serializes the bounded tool result, or a fixed content-free diagnostic when it is too large.
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
        AgentConfigView, ConversationConfigView, ConversationScopeView, EffectiveCapabilityView,
        MAX_AGENT_CONFIG_TOOL_BYTES, SessionConfigView, SkillView,
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
                conversation: ConversationConfigView::Persistent {
                    scope: ConversationScopeView::PrivateConversation,
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
                idempotency: "idempotent".to_owned(),
            }],
        )
    }

    #[test]
    fn view_contains_prompt_limits_and_effective_cedar_grants() {
        let encoded = view("Be concise.".to_owned()).tool_result();
        let value: Value = serde_json::from_str(&encoded).expect("view is JSON");

        assert_eq!(value["agent"]["id"], "reviewer");
        assert_eq!(value["prompt"]["instructions"], "Be concise.");
        assert_eq!(value["session"]["maxSteps"], 8);
        assert_eq!(
            value["session"]["conversation"],
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
            serde_json::to_value(ConversationConfigView::OneShot).expect("view serializes"),
            serde_json::json!({"mode": "oneShot"}),
            "persistent-only fields stay absent from one-shot inspection"
        );
        assert_eq!(
            serde_json::to_value(ConversationConfigView::Persistent {
                scope: ConversationScopeView::SharedConversation,
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

    /// Skills are listed by name and trigger, sorted, and the key is absent when none is mounted.
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
