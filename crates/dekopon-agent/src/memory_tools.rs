use crate::prompt::PromptError;
use dekopon_core::AssetNumber;
use dekopon_model::model::{ModelMessage, ModelTool, ModelToolCall};
use serde::de::Error as _;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachmentTargets {
    Off,
    On,
}

#[derive(Debug, Error, Eq, PartialEq)]
enum TargetError {
    #[error("attachment memory is off")]
    AttachmentsOff,
    #[error("select exactly one of recordId and asset")]
    ExactlyOne,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    Record(u64),
    Asset(AssetNumber),
}

impl Target {
    fn parse(
        attachments: AttachmentTargets,
        record: Option<u64>,
        asset: Option<AssetNumber>,
    ) -> Result<Option<Self>, TargetError> {
        match (attachments, asset) {
            (AttachmentTargets::Off, Some(_)) => return Err(TargetError::AttachmentsOff),
            (AttachmentTargets::Off, None) | (AttachmentTargets::On, _) => {}
        }
        match (record, asset) {
            (Some(record), None) => Ok(Some(Self::Record(record))),
            (None, Some(asset)) => Ok(Some(Self::Asset(asset))),
            (None, None) => Ok(None),
            (Some(_), Some(_)) => Err(TargetError::ExactlyOne),
        }
    }
}

pub trait MemoryTools: Send + Sync {
    fn attachment_targets(&self) -> AttachmentTargets {
        AttachmentTargets::Off
    }
    fn read(
        &self,
        target: Target,
        representation_id: &str,
        range: Option<(u64, u64)>,
    ) -> Result<String, MemoryRefusal>;
    fn set_pin(&self, target: Target, representation_id: &str) -> Result<String, MemoryRefusal>;
    fn clear_pin(&self, target: Option<Target>) -> Result<String, MemoryRefusal>;
}
pub fn tools(attachments: AttachmentTargets) -> [ModelTool; 3] {
    match attachments {
        AttachmentTargets::Off => return record_tools(),
        AttachmentTargets::On => {}
    }
    [
        ModelTool {
            name: "read_memory_record".into(),
            description: "Read a retained recordId or asset number (exactly one). An asset offers generated summary-<n> and metadata; read its original with fetch_chat_asset. representationId is `native` (the original turn), `summary-<n>` (generated summary part n) or `metadata` (the record's envelope). Range is a UTF-8 byte interval [start,end). A summary that was not generated is not available.".into(),
            parameters: json!({"type":"object","properties":{"recordId":{"type":"integer"},"asset":{"type":"integer","minimum":1},"representationId":{"type":"string"},"range":{"type":"array","items":{"type":"integer"},"minItems":2,"maxItems":2}},"required":["representationId"],"oneOf":[{"required":["recordId"]},{"required":["asset"]}],"additionalProperties":false}),
        },
        ModelTool {
            name: "set_memory_pin".into(),
            description: "Pin exactly one recordId or asset number to `native`, `summary-<n>` or `metadata` for future included history; a pin does not force inclusion. An asset native pin requires a live image or binary document that this model can accept; text documents offer summaries and metadata.".into(),
            parameters: json!({"type":"object","properties":{"recordId":{"type":"integer"},"asset":{"type":"integer","minimum":1},"representationId":{"type":"string"}},"required":["representationId"],"oneOf":[{"required":["recordId"]},{"required":["asset"]}],"additionalProperties":false}),
        },
        ModelTool {
            name: "clear_memory_pin".into(),
            description: "Clear exactly one recordId or asset number, or omit both to clear every record and asset pin in this conversation.".into(),
            parameters: json!({"type":"object","properties":{"recordId":{"type":"integer"},"asset":{"type":"integer","minimum":1}},"not":{"required":["recordId","asset"]},"additionalProperties":false}),
        },
    ]
}
fn record_tools() -> [ModelTool; 3] {
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
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RecordArguments {
    #[serde(default, deserialize_with = "present")]
    record_id: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    asset: Option<AssetNumber>,
    representation_id: String,
    #[serde(default)]
    range: Option<(u64, u64)>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ClearArguments {
    #[serde(default, deserialize_with = "present")]
    record_id: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    asset: Option<AssetNumber>,
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
                let target = Target::parse(memory.attachment_targets(), args.record_id, args.asset)
                    .and_then(|target| target.ok_or(TargetError::ExactlyOne))
                    .map_err(|source| PromptError::InvalidArguments {
                        tool: name.to_owned(),
                        source: serde_json::Error::custom(source),
                    })?;
                if name == "read_memory_record" {
                    memory.read(target, &args.representation_id, args.range)
                } else {
                    memory.set_pin(target, &args.representation_id)
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
                let target = Target::parse(memory.attachment_targets(), args.record_id, args.asset)
                    .map_err(|source| PromptError::InvalidArguments {
                        tool: name.to_owned(),
                        source: serde_json::Error::custom(source),
                    })?;
                memory.clear_pin(target)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_memory_off_preserves_the_base_tool_json() {
        let expected = [
            r#"{"name":"read_memory_record","description":"Read a retained record by ID. representationId is `native` (the original turn), `summary-<n>` (generated summary part n) or `metadata` (the record's envelope). Range is a UTF-8 byte interval [start,end). A summary that was not generated is not available.","parameters":{"type":"object","properties":{"recordId":{"type":"integer"},"representationId":{"type":"string"},"range":{"type":"array","items":{"type":"integer"},"minItems":2,"maxItems":2}},"required":["recordId","representationId"],"additionalProperties":false}}"#,
            r#"{"name":"set_memory_pin","description":"Pin a record to `native`, `summary-<n>` or `metadata` for future included history; a pin does not force inclusion.","parameters":{"type":"object","properties":{"recordId":{"type":"integer"},"representationId":{"type":"string"}},"required":["recordId","representationId"],"additionalProperties":false}}"#,
            r#"{"name":"clear_memory_pin","description":"Clear one record's pin, or omit recordId to clear all pins in this conversation.","parameters":{"type":"object","properties":{"recordId":{"type":"integer"}},"additionalProperties":false}}"#,
        ];
        for (tool, expected) in tools(AttachmentTargets::Off).iter().zip(expected) {
            // Workspace policy enables preserve_order; the base fixture must use the active map order.
            let base: serde_json::Value = serde_json::from_str(expected).unwrap();
            let expected = format!(
                r#"{{"name":{},"description":{},"parameters":{}}}"#,
                base["name"], base["description"], base["parameters"]
            );
            assert_eq!(serde_json::to_string(tool).unwrap(), expected);
        }
    }

    #[test]
    fn an_off_attachment_target_is_refused_by_the_parser() {
        let number = AssetNumber::try_from(1).unwrap();
        assert_eq!(
            Target::parse(AttachmentTargets::Off, None, Some(number)),
            Err(TargetError::AttachmentsOff)
        );
    }
}
