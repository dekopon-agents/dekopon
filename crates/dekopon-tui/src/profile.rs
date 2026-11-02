use dekopon_core::{AgentId, Subject};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OperatorProfile {
    pub name: String,
    pub agent: AgentId,
    pub subject: Subject,
    pub model: Option<String>,
    pub max_steps: Option<u32>,
    pub max_capability_calls: Option<u32>,
}
