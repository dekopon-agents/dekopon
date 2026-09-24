//! Purely advisory: nothing changes because a model asked, and it is opt-in per session since
//! enabling it puts model-authored text in scope for the log sink.

use std::fmt;

use dekopon_model::model::{ModelMessage, ModelTool, ModelToolCall};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::prompt::{PromptError, reject_tool_call};

pub const IMPROVEMENT_TOOL_NAME: &str = "suggest_improvement";

/// Three is enough to name the wrong instruction, the missing capability, and the limit that bit;
/// more would be a model narrating, not reporting.
pub const MAX_SUGGESTIONS_PER_SESSION: usize = 3;
pub const MAX_SUGGESTION_TARGET_BYTES: usize = 128;
pub const MAX_SUGGESTION_SUMMARY_BYTES: usize = 512;
pub const MAX_SUGGESTION_DETAIL_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImprovementCategory {
    Instructions,
    Skill,
    Capability,
    Tool,
    Limits,
    Other,
}

impl ImprovementCategory {
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SuggestionConfidence {
    Low,
    Medium,
    High,
}

impl SuggestionConfidence {
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImprovementSuggestion {
    pub category: ImprovementCategory,
    pub target: String,
    pub summary: String,
    pub evidence: String,
    pub proposal: String,
    pub confidence: SuggestionConfidence,
}

/// Fields are plain strings, not typed enums, so a bad token or oversized value becomes a refusal
/// the model can fix, not a decode failure that ends the session.
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

pub(crate) fn improvement_tool() -> ModelTool {
    ModelTool {
        name: IMPROVEMENT_TOOL_NAME.to_owned(),
        description: format!(
            "Tap the glass: record one structured note telling the operator how this agent could \
             be improved. Use it when you noticed something the operator could fix: standing \
             instructions that were wrong, missing, or contradictory; a skill that would have \
             helped or that misled you; a capability you needed but were not granted; a limit \
             you ran into; a tool that behaved differently from how it was described. Before \
             recording one, ask whether a future session of this agent would plausibly act better \
             because of it: skip one-off facts, live values a script should query again, and \
             anything your instructions already say. Ground every field in something you observed \
             in this session's tool results, such as an exit code, a refusal message, or a missing \
             fact, rather than in speculation. Call it \
             after the task is done or when it is genuinely blocked, at most {MAX_SUGGESTIONS_PER_SESSION} \
             times per session, never instead of answering, and without asking the person for \
             permission. Recording a note changes nothing in this session; it goes to the \
             operator's telemetry, not to the person you are talking with, so be specific: name \
             the thing in `target`, quote the evidence briefly, and propose one concrete change. \
             Returns a confirmation with the note's number out of {MAX_SUGGESTIONS_PER_SESSION}; \
             a note that breaks a bound is refused with the reason, so fix it and resend, or \
             continue without it."
        ),
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
    // This record always includes the model-authored text regardless of the telemetry setting,
    // since offering the tool is itself the opt-in, but never the chat text or the subject.
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
