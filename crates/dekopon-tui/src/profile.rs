//! Operator-authored context selectors. These are claims, never grants.

use dekopon_core::{AgentId, ExternalSubject};
use serde::Deserialize;

/// A named console context, validated by the CLI before a connection is opened.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OperatorProfile {
    pub name: String,
    pub agent: AgentId,
    pub subject: ExternalSubject,
    pub model: Option<String>,
    pub max_steps: Option<u32>,
    pub max_capability_calls: Option<u32>,
}
