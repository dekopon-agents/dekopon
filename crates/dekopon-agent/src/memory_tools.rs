use crate::prompt::PromptError;
use dekopon_model::model::{ModelMessage, ModelTool, ModelToolCall};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{fmt, io, num::NonZeroU8, str::FromStr};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rung {
    Native,
    Summary(NonZeroU8),
    Metadata,
}

impl fmt::Display for Rung {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Native => formatter.write_str("native"),
            Self::Summary(part) => write!(formatter, "summary-{part}"),
            Self::Metadata => formatter.write_str("metadata"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownRung;

impl FromStr for Rung {
    type Err = UnknownRung;

    fn from_str(id: &str) -> Result<Self, Self::Err> {
        match id {
            "native" => Ok(Self::Native),
            "metadata" => Ok(Self::Metadata),
            _ => id
                .strip_prefix("summary-")
                .filter(|part| !part.starts_with(['0', '+']))
                .and_then(|part| part.parse().ok())
                .map(Self::Summary)
                .ok_or(UnknownRung),
        }
    }
}

fn ids(available: &[Rung]) -> String {
    available
        .iter()
        .map(Rung::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Error)]
pub enum MemoryRefusal {
    #[error("record unavailable: expired, evicted or unknown")]
    RecordUnknown,
    #[error("record unavailable: expired or changed grant")]
    RecordIneligible,
    #[error("record unavailable: storage error")]
    Storage { kind: io::ErrorKind },
    #[error(
        "representation unavailable: unknown representation; eligible representation IDs: {}",
        ids(available)
    )]
    UnknownRepresentation { available: Vec<Rung> },
    #[error(
        "representation unavailable: missing, discarded or not generated; eligible representation IDs: {}",
        ids(available)
    )]
    RepresentationUnavailable { available: Vec<Rung> },
    #[error("range unavailable: select at most 16384 UTF-8 bytes (0..{len})")]
    Range { len: u64 },
}

pub trait MemoryTools: Send + Sync {
    fn read(
        &self,
        record_id: u64,
        representation_id: &str,
        range: Option<(u64, u64)>,
    ) -> Result<String, MemoryRefusal>;
    fn set_pin(&self, record_id: u64, representation_id: &str) -> Result<String, MemoryRefusal>;
    fn clear_pin(&self, record_id: Option<u64>) -> Result<String, MemoryRefusal>;
}
pub fn tools() -> [ModelTool; 3] {
    [
        ModelTool {
            name: "read_memory_record".into(),
            description: "Read a retained record by ID. representationId is `native` (the original turn), `summary-<n>` (generated summary part n) or `metadata` (the record's envelope). Range is a UTF-8 byte interval [start,end). A summary that was not generated is not available.".into(),
            parameters: json!({"type":"object","properties":{"recordId":{"type":"integer"},"representationId":{"type":"string"},"range":{"type":"array","items":{"type":"integer"},"minItems":2,"maxItems":2}},"required":["recordId","representationId"],"additionalProperties":false}),
        },
        ModelTool {
            name: "set_memory_pin".into(),
            description: "Pin a record to `native`, `summary-<n>` or `metadata` for future included history; a pin does not force inclusion.".into(),
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
pub enum Dispatched {
    NotMemory,
    Served,
    Refused,
}

pub fn dispatch(
    messages: &mut Vec<ModelMessage>,
    memory: &dyn MemoryTools,
    call: &ModelToolCall,
) -> Result<Dispatched, PromptError> {
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
            _ => return Ok(Dispatched::NotMemory),
        };
    let (text, dispatched) = match result {
        Ok(text) => (text, Dispatched::Served),
        Err(refusal) => (refusal.to_string(), Dispatched::Refused),
    };
    messages.push(ModelMessage::tool(call.id.clone(), text));
    Ok(dispatched)
}
