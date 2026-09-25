use std::{fmt, time::Duration};

use dekopon_model::model::{ModelMessage, ModelTool, ModelToolCall};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;

use crate::prompt::{PromptError, reject_tool_call};

pub const WAKE_TOOL_NAME: &str = "wake";

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WakeId(pub u32);

impl fmt::Display for WakeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WakeRequest {
    Once {
        note: String,
        after: Duration,
    },
    Watch {
        note: String,
        script: String,
        every: Duration,
        until: Duration,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WakeSummary {
    pub id: WakeId,
    pub note: String,
    pub due_in: Duration,
    pub watch: Option<WatchSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchSummary {
    pub every: Duration,
    pub ends_in: Duration,
    pub last_output: String,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WakeRefusal {
    #[error("the note is longer than {maximum} bytes")]
    NoteTooLong { maximum: usize },
    #[error("that is further out than this chat allows ({} seconds at most)", maximum.as_secs())]
    Horizon { maximum: Duration },
    #[error("a watch may check at most once every {} seconds", minimum.as_secs())]
    Interval { minimum: Duration },
    #[error("this person already has {maximum} pending wakes; cancel one first")]
    Full { maximum: usize },
    #[error("the probe script exited {exit} on its first run, so it was not stored:\n{output}")]
    Broken { exit: u8, output: String },
    #[error("no pending wake of this person has that id")]
    NotFound,
    #[error("the gateway could not reach the broker to run the probe")]
    Unavailable,
    #[error("the gateway could not save the wake")]
    Store,
}

impl WakeRefusal {
    #[must_use]
    pub const fn telemetry_kind(&self) -> &'static str {
        match self {
            Self::NoteTooLong { .. } => "note-too-long",
            Self::Horizon { .. } => "horizon",
            Self::Interval { .. } => "interval",
            Self::Full { .. } => "full",
            Self::Broken { .. } => "broken-probe",
            Self::NotFound => "not-found",
            Self::Unavailable => "unavailable",
            Self::Store => "store",
        }
    }
}

pub trait WakeRegistrar: Send + Sync {
    fn schedule(&self, request: WakeRequest) -> Result<WakeSummary, WakeRefusal>;

    fn list(&self) -> Vec<WakeSummary>;

    fn cancel(&self, id: WakeId) -> Result<(), WakeRefusal>;
}

#[derive(Deserialize)]
#[serde(
    tag = "action",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum WakeCall {
    Schedule {
        note: String,
        after_seconds: u64,
    },
    Watch {
        note: String,
        script: String,
        every_seconds: u64,
        for_seconds: u64,
    },
    List {},
    Cancel {
        id: u32,
    },
}

pub(crate) fn wake_tool() -> ModelTool {
    ModelTool {
        name: WAKE_TOOL_NAME.to_owned(),
        description: "Come back to this conversation later, as the person who asked, in this \
                      same chat. This is the only way to schedule, see or cancel a wake. \
                      `schedule` wakes you once after `afterSeconds` with your `note`. `watch` \
                      runs `script` in the same shell as `bash` every `everySeconds` for up to \
                      `forSeconds`, with no model in the loop: exit 0 wakes you with its output, \
                      exit 1 keeps waiting, and any other exit wakes you with the failure. The \
                      previous run's output is in `$PREV`; it is unset on the first run, which \
                      happens now and never wakes you. A watch script may only read: every \
                      capability that writes is refused to it. The note is what you will see \
                      when you wake, so say what to do then. `list` shows this person's pending \
                      wakes and `cancel` removes one by id."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["schedule", "watch", "list", "cancel"] },
                "note": { "type": "string", "description": "What to do when you wake." },
                "afterSeconds": { "type": "integer", "minimum": 1 },
                "script": { "type": "string", "description": "The watch probe." },
                "everySeconds": { "type": "integer", "minimum": 1 },
                "forSeconds": { "type": "integer", "minimum": 1 },
                "id": { "type": "integer", "description": "The wake to cancel." }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn wake_into(
    messages: &mut Vec<ModelMessage>,
    registrar: &dyn WakeRegistrar,
    call: &ModelToolCall,
    model_turn: u32,
    tool_call_index: usize,
) -> Result<(), PromptError> {
    let request = match serde_json::from_str::<WakeCall>(&call.function.arguments) {
        Ok(request) => request,
        Err(source) => {
            let error = PromptError::InvalidWake {
                tool: call.function.name.clone(),
                source,
            };
            reject_tool_call(model_turn, tool_call_index, error.telemetry_kind());
            return Err(error);
        }
    };
    let result = match request {
        WakeCall::Schedule {
            note,
            after_seconds,
        } => registrar
            .schedule(WakeRequest::Once {
                note,
                after: Duration::from_secs(after_seconds),
            })
            .map(|summary| format!("Scheduled.\n{}", render(&summary))),
        WakeCall::Watch {
            note,
            script,
            every_seconds,
            for_seconds,
        } => registrar
            .schedule(WakeRequest::Watch {
                note,
                script,
                every: Duration::from_secs(every_seconds),
                until: Duration::from_secs(for_seconds),
            })
            .map(|summary| format!("Watching.\n{}", render(&summary))),
        WakeCall::List {} => Ok(match registrar.list().as_slice() {
            [] => "No pending wakes.".to_owned(),
            summaries => summaries
                .iter()
                .map(render)
                .collect::<Vec<_>>()
                .join("\n\n"),
        }),
        WakeCall::Cancel { id } => registrar
            .cancel(WakeId(id))
            .map(|()| format!("Cancelled wake {id}.")),
    };
    let text = match result {
        Ok(text) => text,
        Err(refusal) => {
            tracing::info!(
                target: "dekopon_agent::audit",
                {
                    audit.event = "agent.wake.refused",
                    model.turn = model_turn,
                    tool_call.index = tool_call_index,
                    reason = refusal.telemetry_kind(),
                },
                "wake refused"
            );
            format!("Not done: {refusal}.")
        }
    };
    messages.push(ModelMessage::tool(call.id.clone(), text));
    Ok(())
}

fn render(summary: &WakeSummary) -> String {
    let mut text = format!(
        "wake {}: next in {} seconds\nnote: {}",
        summary.id,
        summary.due_in.as_secs(),
        summary.note
    );
    if let Some(watch) = &summary.watch {
        text.push_str(&format!(
            "\nchecks every {} seconds, gives up in {} seconds\nlast output:\n{}",
            watch.every.as_secs(),
            watch.ends_in.as_secs(),
            watch.last_output
        ));
    }
    text
}
