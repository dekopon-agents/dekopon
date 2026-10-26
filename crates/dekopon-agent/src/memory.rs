use dekopon_core::AssetNumber;
use dekopon_model::model::{
    AssistantTurn, ModelKey, ModelMessage, ModelToolCall, OpaqueReasoning, ToolCallId,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kept {
    text: String,
    original_bytes: u64,
}

impl Kept {
    pub fn new(text: &str, cap: usize) -> Self {
        Self {
            text: text[..text.floor_char_boundary(cap.min(text.len()))].to_owned(),
            original_bytes: text.len() as u64,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn original_bytes(&self) -> u64 {
        self.original_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Item {
    User(Kept),
    Steer(Kept),
    Assistant(Kept),
    Reasoning(Reasoning),
    Exchange(Exchange),
    Asset(AssetNumber),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Reasoning {
    Plain(Kept),
    Opaque {
        model: ModelKey,
        bytes: OpaqueReasoning,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exchange {
    pub call: Call,
    pub result: ToolResult,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub id: ToolCallId,
    pub tool: Tool,
    pub arguments: Kept,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Tool {
    Bash,
    ReadSkill,
    FetchChatAsset,
    ReadMemoryRecord,
    SetMemoryPin,
    ClearMemoryPin,
    InspectAgentConfig,
    SuggestImprovement,
    Wake,
    Rejected(Kept),
}

impl Tool {
    fn named(name: &str, cap: usize) -> Self {
        match name {
            "bash" => Self::Bash,
            "read_skill" => Self::ReadSkill,
            "fetch_chat_asset" => Self::FetchChatAsset,
            "read_memory_record" => Self::ReadMemoryRecord,
            "set_memory_pin" => Self::SetMemoryPin,
            "clear_memory_pin" => Self::ClearMemoryPin,
            "inspect_agent_config" => Self::InspectAgentConfig,
            "suggest_improvement" => Self::SuggestImprovement,
            "wake" => Self::Wake,
            _ => Self::Rejected(Kept::new(name, cap)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub outcome: Outcome,
    pub text: Kept,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    Exit(u8),
    Refused,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedTurn {
    pub original: Vec<Item>,
    pub model: ModelKey,
}

pub(crate) struct RecordBuilder {
    original: Vec<Item>,
    model: ModelKey,
    cap: usize,
}

impl RecordBuilder {
    pub(crate) fn new(user: &str, model: ModelKey, cap: usize) -> Self {
        Self {
            original: vec![Item::User(Kept::new(user, cap))],
            model,
            cap,
        }
    }

    pub(crate) fn steer(&mut self, recorded: &str) {
        self.original
            .push(Item::Steer(Kept::new(recorded, self.cap)));
    }

    pub(crate) fn assistant(&mut self, turn: &AssistantTurn) {
        if let Some(model) = turn.model_key() {
            self.model = model.clone();
        }
        match turn.opaque_reasoning() {
            Ok(items) => {
                for bytes in items {
                    if bytes.bytes().len() <= 64 * 1024 {
                        self.original.push(Item::Reasoning(Reasoning::Opaque {
                            model: self.model.clone(),
                            bytes,
                        }));
                    } else {
                        tracing::warn!(
                            memory.opaque.dropped = 1,
                            memory.opaque.bytes = bytes.bytes().len(),
                            "memory opaque capture exceeded its cap"
                        );
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "memory opaque capture failed"),
        }
        if let Some(text) = &turn.content {
            self.original
                .push(Item::Assistant(Kept::new(text, self.cap)));
        }
    }

    pub(crate) fn exchange(&mut self, call: &ModelToolCall, text: &str, outcome: Outcome) {
        self.original.push(Item::Exchange(Exchange {
            call: Call {
                id: call.id.clone(),
                tool: Tool::named(&call.function.name, self.cap),
                arguments: Kept::new(&call.function.arguments, self.cap),
            },
            result: ToolResult {
                outcome,
                text: Kept::new(text, self.cap),
            },
        }));
    }

    pub(crate) fn result(
        &mut self,
        call: &ModelToolCall,
        messages: &[ModelMessage],
        outcome: Outcome,
    ) {
        if let Some(text) = messages.iter().rev().find_map(|message| match message {
            ModelMessage::ToolResults {
                content,
                tool_call_id,
            } if tool_call_id == &call.id => Some(content.as_str()),
            ModelMessage::ToolResults { .. }
            | ModelMessage::User { .. }
            | ModelMessage::Assistant { .. }
            | ModelMessage::System { .. } => None,
        }) {
            self.exchange(call, text, outcome);
        }
    }

    pub(crate) fn finish(self) -> CapturedTurn {
        CapturedTurn {
            original: self.original,
            model: self.model,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_model::model::{Backend, ModelFunctionCall};

    #[test]
    fn capture_keeps_exact_arguments_and_caps_results_at_utf8_boundaries() {
        let mut builder = RecordBuilder::new("user", ModelKey::new(Backend::Codex, "model"), 3);
        let call = ModelToolCall {
            id: "call".into(),
            kind: "function".into(),
            function: ModelFunctionCall {
                name: "bash".into(),
                arguments: " {}".into(),
            },
        };
        builder.exchange(&call, "éé", Outcome::Failed);
        let record = builder.finish();
        let Item::Exchange(exchange) = &record.original[1] else {
            panic!("expected exchange")
        };
        assert_eq!(exchange.call.arguments.text(), " {}");
        assert_eq!(exchange.result.text.text(), "é");
        assert_eq!(exchange.result.text.original_bytes(), 4);
        assert_eq!(exchange.result.outcome, Outcome::Failed);
    }

    #[test]
    fn an_asset_item_has_only_its_numeric_marker() {
        let item = Item::Asset(AssetNumber::try_from(7).unwrap());
        assert_eq!(serde_json::to_string(&item).unwrap(), r#"{"asset":7}"#);
    }
}
