//! Tapping the glass: a bounded channel for an agent to tell its operator how to improve it.
//!
//! An agent that hit a limit, reached for a capability it was never granted, or found its standing
//! instructions wrong has learned something its operator would pay to know, and today it can say
//! so only in chat, to a person who may not be the operator. This tool gives that observation a
//! typed shape and a tagged telemetry record, so an operator can aggregate a month of sessions by
//! category and target rather than reading transcripts.
//!
//! It is advisory by construction. A suggestion changes nothing: no instruction, skill, limit, or
//! grant moves because a model asked. It is recorded, and a person decides. The channel is also
//! opt-in per session, because the record carries model-authored text and enabling it is what
//! declares the log sink in scope for that text.

use std::fmt;

use dekopon_model::model::{ModelMessage, ModelTool, ModelToolCall};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::prompt::{PromptError, reject_tool_call};

/// The tool a model calls to record one improvement suggestion.
pub const IMPROVEMENT_TOOL_NAME: &str = "suggest_improvement";

/// Suggestions one session may record.
///
/// Three is enough to name the instruction that was wrong, the capability that was missing, and
/// the limit that bit; more than that is a model narrating rather than reporting.
pub const MAX_SUGGESTIONS_PER_SESSION: usize = 3;
/// Bytes one `target` may carry: a skill name, a capability identifier, a limit name.
pub const MAX_SUGGESTION_TARGET_BYTES: usize = 128;
/// Bytes one `summary` may carry: a sentence.
pub const MAX_SUGGESTION_SUMMARY_BYTES: usize = 512;
/// Bytes `evidence` and `proposal` may each carry.
pub const MAX_SUGGESTION_DETAIL_BYTES: usize = 2048;

/// What kind of operator-owned thing a suggestion is about.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImprovementCategory {
    /// The agent's standing instructions.
    Instructions,
    /// A mounted skill, or one that should exist.
    Skill,
    /// A capability the agent holds, or one it needed and lacked.
    Capability,
    /// The scripting tool, a builtin, or another tool's behavior.
    Tool,
    /// A step, capability, output, or time bound.
    Limits,
    /// Anything else.
    Other,
}

impl ImprovementCategory {
    /// The stable wire and telemetry token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Instructions => "instructions",
            Self::Skill => "skill",
            Self::Capability => "capability",
            Self::Tool => "tool",
            Self::Limits => "limits",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for ImprovementCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How sure the model is that the change would help.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SuggestionConfidence {
    /// A hunch.
    Low,
    /// Likely, from one session's evidence.
    Medium,
    /// The session demonstrated it.
    High,
}

impl SuggestionConfidence {
    /// The stable wire and telemetry token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl fmt::Display for SuggestionConfidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One recorded suggestion: bounded, sanitized, and typed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImprovementSuggestion {
    /// What kind of thing the operator would change.
    pub category: ImprovementCategory,
    /// The specific thing: a skill name, a capability identifier, `instructions`, a limit.
    pub target: String,
    /// One sentence: what was wrong or could be better.
    pub summary: String,
    /// What the session observed that supports it.
    pub evidence: String,
    /// The concrete change proposed.
    pub proposal: String,
    /// How sure the model is.
    pub confidence: SuggestionConfidence,
}

/// The shape the model sends, before any bound is checked.
///
/// Every field is a plain string here so that a wrong enum token or an oversized value becomes a
/// refusal the model reads rather than a decode failure that ends the session: a suggestion is
/// advisory, and the task it was about must not fail because the note was formatted badly.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSuggestion {
    category: String,
    target: String,
    summary: String,
    evidence: String,
    proposal: String,
    confidence: String,
}

/// Builds the suggestion tool.
pub(crate) fn improvement_tool() -> ModelTool {
    ModelTool {
        name: IMPROVEMENT_TOOL_NAME.to_owned(),
        description: "Tap the glass: record one structured note telling the operator how this \
                      agent could be improved. Use it when you noticed something the operator \
                      could fix — standing instructions that were wrong, missing, or contradictory; \
                      a skill that would have helped or that misled you; a capability you needed \
                      but were not granted; a limit you ran into; a tool that behaved differently \
                      from how it was described. Call it after the task is done or when it is \
                      genuinely blocked, at most three times per session, and never instead of \
                      answering. The note goes to the operator's telemetry, not to the person you \
                      are talking with, so be specific: name the thing, quote the evidence briefly, \
                      and propose one concrete change."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "category": {
                    "type": "string",
                    "enum": ["instructions", "skill", "capability", "tool", "limits", "other"],
                    "description": "What kind of thing the operator would change."
                },
                "target": {
                    "type": "string",
                    "maxLength": MAX_SUGGESTION_TARGET_BYTES,
                    "description": "The specific thing: a skill name, a capability identifier, `instructions`, a limit name, a builtin."
                },
                "summary": {
                    "type": "string",
                    "maxLength": MAX_SUGGESTION_SUMMARY_BYTES,
                    "description": "One sentence: what was wrong or could be better."
                },
                "evidence": {
                    "type": "string",
                    "maxLength": MAX_SUGGESTION_DETAIL_BYTES,
                    "description": "What you observed in this session that supports it: an exit code, a refusal, a missing fact."
                },
                "proposal": {
                    "type": "string",
                    "maxLength": MAX_SUGGESTION_DETAIL_BYTES,
                    "description": "The concrete change: the instruction to add, the skill to write, the capability to grant, the limit to raise."
                },
                "confidence": {
                    "type": "string",
                    "enum": ["low", "medium", "high"],
                    "description": "How sure you are that the change would help."
                }
            },
            "required": ["category", "target", "summary", "evidence", "proposal", "confidence"],
            "additionalProperties": false
        }),
    }
}

/// Answers one `suggest_improvement` call, recording the suggestion when it is well formed.
///
/// Malformed JSON or a non-object ends the session as it does for every tool. A well-formed object
/// that fails a bound is answered with the bound, so the model can shorten and resubmit or move on.
pub(crate) fn suggest_improvement_into(
    messages: &mut Vec<ModelMessage>,
    suggestions: &mut Vec<ImprovementSuggestion>,
    call: &ModelToolCall,
    model_turn: u32,
    tool_call_index: usize,
) -> Result<(), PromptError> {
    let raw = match raw_suggestion(&call.function.name, &call.function.arguments) {
        Ok(raw) => raw,
        Err(error) => {
            reject_tool_call(model_turn, tool_call_index, error.telemetry_kind());
            return Err(error);
        }
    };
    if suggestions.len() >= MAX_SUGGESTIONS_PER_SESSION {
        refuse(model_turn, tool_call_index, "session-limit");
        messages.push(ModelMessage::tool(
            call.id.clone(),
            format!(
                "This session has already recorded its {MAX_SUGGESTIONS_PER_SESSION} suggestions; \
                 this one was not recorded. Continue with the task."
            ),
        ));
        return Ok(());
    }
    let suggestion = match validate(raw) {
        Ok(suggestion) => suggestion,
        Err((reason, message)) => {
            refuse(model_turn, tool_call_index, reason);
            messages.push(ModelMessage::tool(
                call.id.clone(),
                format!("Suggestion not recorded: {message} Fix it and call again, or continue."),
            ));
            return Ok(());
        }
    };
    let index = suggestions.len() + 1;
    // The text fields are model-authored, and they are recorded whether or not payload telemetry
    // is on: offering this tool is the operator's opt-in, and a suggestion nobody can read is not
    // a suggestion. What the record never carries is chat text the gateway holds or a subject —
    // only what the model chose to write into these six bounded fields.
    tracing::info!(
        target: "dekopon_agent::audit",
        {
            audit.event = "agent.improvement.suggested",
            model.turn = model_turn,
            tool_call.index = tool_call_index,
            suggestion.index = index,
            suggestion.category = suggestion.category.as_str(),
            suggestion.confidence = suggestion.confidence.as_str(),
            suggestion.target = suggestion.target.as_str(),
            suggestion.summary = suggestion.summary.as_str(),
            suggestion.evidence = suggestion.evidence.as_str(),
            suggestion.proposal = suggestion.proposal.as_str(),
        },
        "agent improvement suggested"
    );
    suggestions.push(suggestion);
    messages.push(ModelMessage::tool(
        call.id.clone(),
        format!(
            "Recorded suggestion {index} of {MAX_SUGGESTIONS_PER_SESSION} for the operator. \
             Continue with the task, or finish."
        ),
    ));
    Ok(())
}

fn refuse(model_turn: u32, tool_call_index: usize, reason: &'static str) {
    tracing::info!(
        target: "dekopon_agent::audit",
        {
            audit.event = "agent.improvement.refused",
            model.turn = model_turn,
            tool_call.index = tool_call_index,
            reason = reason,
        },
        "agent improvement suggestion refused"
    );
}

fn raw_suggestion(tool: &str, arguments: &str) -> Result<RawSuggestion, PromptError> {
    let value = serde_json::from_str::<Value>(arguments).map_err(|source| {
        PromptError::InvalidArguments {
            tool: tool.to_owned(),
            source,
        }
    })?;
    if !value.is_object() {
        return Err(PromptError::ArgumentsNotObject {
            tool: tool.to_owned(),
        });
    }
    serde_json::from_value::<RawSuggestion>(value).map_err(|source| {
        PromptError::InvalidSuggestion {
            tool: tool.to_owned(),
            source,
        }
    })
}

/// Checks every bound, returning the telemetry reason and the sentence the model reads.
fn validate(raw: RawSuggestion) -> Result<ImprovementSuggestion, (&'static str, String)> {
    let category = match raw.category.trim() {
        "instructions" => ImprovementCategory::Instructions,
        "skill" => ImprovementCategory::Skill,
        "capability" => ImprovementCategory::Capability,
        "tool" => ImprovementCategory::Tool,
        "limits" => ImprovementCategory::Limits,
        "other" => ImprovementCategory::Other,
        _ => {
            return Err((
                "invalid-category",
                "`category` must be one of instructions, skill, capability, tool, limits, other."
                    .to_owned(),
            ));
        }
    };
    let confidence = match raw.confidence.trim() {
        "low" => SuggestionConfidence::Low,
        "medium" => SuggestionConfidence::Medium,
        "high" => SuggestionConfidence::High,
        _ => {
            return Err((
                "invalid-confidence",
                "`confidence` must be one of low, medium, high.".to_owned(),
            ));
        }
    };
    let target = bounded("target", &raw.target, MAX_SUGGESTION_TARGET_BYTES)?;
    let summary = bounded("summary", &raw.summary, MAX_SUGGESTION_SUMMARY_BYTES)?;
    let evidence = bounded("evidence", &raw.evidence, MAX_SUGGESTION_DETAIL_BYTES)?;
    let proposal = bounded("proposal", &raw.proposal, MAX_SUGGESTION_DETAIL_BYTES)?;
    Ok(ImprovementSuggestion {
        category,
        target,
        summary,
        evidence,
        proposal,
        confidence,
    })
}

/// Trims, strips control characters that could forge log structure, and enforces one bound.
fn bounded(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<String, (&'static str, String)> {
    let cleaned = value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>()
        .trim()
        .to_owned();
    if cleaned.is_empty() {
        return Err(("empty-field", format!("`{field}` must not be empty.")));
    }
    if cleaned.len() > maximum {
        return Err((
            "field-too-long",
            format!(
                "`{field}` is {} bytes; the maximum is {maximum}.",
                cleaned.len()
            ),
        ));
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::{
        ImprovementCategory, MAX_SUGGESTION_DETAIL_BYTES, RawSuggestion, SuggestionConfidence,
        raw_suggestion, validate,
    };

    fn raw(category: &str, confidence: &str) -> RawSuggestion {
        RawSuggestion {
            category: category.to_owned(),
            target: "gh.pull-request.read".to_owned(),
            summary: "The read capability was not granted.".to_owned(),
            evidence: "exit code 127 on every attempt".to_owned(),
            proposal: "Grant gh.pull-request.read to this agent.".to_owned(),
            confidence: confidence.to_owned(),
        }
    }

    #[test]
    fn a_well_formed_suggestion_validates() {
        let suggestion = validate(raw("capability", "high")).expect("valid");
        assert_eq!(suggestion.category, ImprovementCategory::Capability);
        assert_eq!(suggestion.confidence, SuggestionConfidence::High);
        assert_eq!(suggestion.target, "gh.pull-request.read");
    }

    /// Every refusal names its reason, because the model has to be able to fix what it sent.
    #[test]
    fn bounds_and_tokens_are_refused_by_reason() {
        assert_eq!(
            validate(raw("bogus", "high")).unwrap_err().0,
            "invalid-category"
        );
        assert_eq!(
            validate(raw("tool", "certain")).unwrap_err().0,
            "invalid-confidence"
        );
        let mut empty = raw("tool", "low");
        empty.target = "  \n".to_owned();
        assert_eq!(validate(empty).unwrap_err().0, "empty-field");
        let mut long = raw("tool", "low");
        long.proposal = "p".repeat(MAX_SUGGESTION_DETAIL_BYTES + 1);
        let (reason, message) = validate(long).unwrap_err();
        assert_eq!(reason, "field-too-long");
        assert!(message.contains("proposal"), "{message}");
    }

    #[test]
    fn control_characters_are_stripped_before_the_record_is_written() {
        let mut noisy = raw("other", "medium");
        noisy.summary = "line one\u{1b}[31m\r\n".to_owned();
        let suggestion = validate(noisy).expect("valid after cleaning");
        assert_eq!(suggestion.summary, "line one[31m");
    }

    #[test]
    fn arguments_must_be_a_json_object_of_the_six_fields() {
        assert!(raw_suggestion("suggest_improvement", "not json").is_err());
        assert!(raw_suggestion("suggest_improvement", "[]").is_err());
        assert!(raw_suggestion("suggest_improvement", r#"{"category":"tool"}"#).is_err());
        assert!(
            raw_suggestion(
                "suggest_improvement",
                r#"{"category":"tool","target":"t","summary":"s","evidence":"e","proposal":"p","confidence":"low","extra":1}"#
            )
            .is_err()
        );
        assert!(
            raw_suggestion(
                "suggest_improvement",
                r#"{"category":"tool","target":"t","summary":"s","evidence":"e","proposal":"p","confidence":"low"}"#
            )
            .is_ok()
        );
    }
}
