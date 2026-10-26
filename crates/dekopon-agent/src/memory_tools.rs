use crate::prompt::PromptError;
use dekopon_model::model::{ModelMessage, ModelTool, ModelToolCall};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rung {
    Native,
    Summary(std::num::NonZeroU8),
    Metadata,
}

pub trait MemoryTools: Send + Sync {
    fn read(&self, record_id: u64, representation_id: &str, range: Option<(u64, u64)>) -> String;
    fn set_pin(&self, record_id: u64, representation_id: &str) -> String;
    fn clear_pin(&self, record_id: Option<u64>) -> String;
}
pub fn tools() -> [ModelTool; 3] {
    [
        ModelTool {
            name: "read_memory_record".into(),
            description: "Read eligible retained text by record and representation ID. Range is a UTF-8 byte interval [start,end). Unavailable detail is not regenerated.".into(),
            parameters: json!({"type":"object","properties":{"recordId":{"type":"integer"},"representationId":{"type":"string"},"range":{"type":"array","items":{"type":"integer"},"minItems":2,"maxItems":2}},"required":["recordId","representationId"],"additionalProperties":false}),
        },
        ModelTool {
            name: "set_memory_pin".into(),
            description: "Select an eligible record's representation for future included history; a pin does not force inclusion.".into(),
            parameters: json!({"type":"object","properties":{"recordId":{"type":"integer"},"representationId":{"type":"string"}},"required":["recordId","representationId"],"additionalProperties":false}),
        },
        ModelTool {
            name: "clear_memory_pin".into(),
            description: "Clear one record's pin, or omit recordId to clear all pins in this conversation.".into(),
            parameters: json!({"type":"object","properties":{"recordId":{"type":"integer"}},"additionalProperties":false}),
        },
    ]
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RecordArguments {
    record_id: u64,
    representation_id: String,
    #[serde(default)]
    range: Option<(u64, u64)>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ClearArguments {
    #[serde(default)]
    record_id: Option<u64>,
}
pub fn dispatch(
    messages: &mut Vec<ModelMessage>,
    memory: &dyn MemoryTools,
    call: &ModelToolCall,
) -> Result<bool, PromptError> {
    let name = call.function.name.as_str();
    let result =
        match name {
            "read_memory_record" | "set_memory_pin" => {
                let args: RecordArguments = serde_json::from_str(&call.function.arguments)
                    .map_err(|source| PromptError::InvalidArguments {
                        tool: name.to_owned(),
                        source,
                    })?;
                if name == "read_memory_record" {
                    memory.read(args.record_id, &args.representation_id, args.range)
                } else {
                    memory.set_pin(args.record_id, &args.representation_id)
                }
            }
            "clear_memory_pin" => {
                let args: ClearArguments =
                    serde_json::from_str(&call.function.arguments).map_err(|source| {
                        PromptError::InvalidArguments {
                            tool: name.to_owned(),
                            source,
                        }
                    })?;
                memory.clear_pin(args.record_id)
            }
            _ => return Ok(false),
        };
    messages.push(ModelMessage::tool(call.id.clone(), result));
    Ok(true)
}
