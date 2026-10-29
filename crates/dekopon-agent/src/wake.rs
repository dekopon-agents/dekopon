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
    Digest {
        note: String,
        script: String,
        every: Duration,
        until: Duration,
    },
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
    #[error("this space already has a daily digest; cancel it first")]
    DigestExists,
    #[error("this agent already has {maximum} PR subscriptions; cancel one first")]
    SubscriptionsFull { maximum: usize },
    #[error("space wakes require an authorized participant in a mapped channel")]
    MappedChannelRequired,
    #[error("the note is longer than {maximum} bytes")]
    NoteTooLong { maximum: usize },
    #[error("that is further out than this chat allows ({} seconds at most)", maximum.as_secs())]
    Horizon { maximum: Duration },
    #[error(
        "a watch checks at most once every {} seconds, and at least once before it gives up",
        minimum.as_secs()
    )]
    Interval { minimum: Duration },
    #[error("this subject already has {maximum} pending wakes; cancel one first")]
    Full { maximum: usize },
    #[error("the probe script exited {exit} on its first run, so it was not stored:\n{output}")]
    Broken { exit: u8, output: String },
    #[error("no pending wake has that id in this scope")]
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
            Self::DigestExists => "digest-exists",
            Self::SubscriptionsFull { .. } => "subscriptions-full",
            Self::MappedChannelRequired => "mapped-channel-required",
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
    fn list_subscriptions(&self) -> Result<Vec<WakeSummary>, WakeRefusal>;

    fn cancel_subscription(&self, id: WakeId) -> Result<(), WakeRefusal>;
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
    Digest {
        note: String,
        script: String,
        every_seconds: u64,
        for_seconds: u64,
    },
    ListSubscriptions {},
    CancelSubscription {
        id: u32,
    },
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
        description: "Come back to this chat later. `schedule` wakes you once after \
                      `afterSeconds` with your `note`. `watch` runs a read-only `script` every \
                      `everySeconds` for up to `forSeconds`, with no model: exit 0 wakes you \
                      with its output, exit 1 keeps waiting, any other exit wakes you with the \
                      failure. `$PREV` holds the previous run's output; the first run is now and \
                      only sets it. In mapped channels all new wakes run as the space; anyone freshly authorized \
                      in the channel can list/cancel them. DMs keep personal wakes. \
                      `digest` uses the watch arguments for a recurring daily digest in a mapped channel, \
                      at least 86400 seconds apart, for at most the configured horizon (30 days by default); \
                      state its expiry, then require a new request to renew it. Only one digest per space. \
                      Its read-only script prints a stable sorted snapshot of open non-draft PRs inactive \
                      seven days, outstanding review requests without an answering review, and open \
                      unassigned issues, using granted repositories and plain GitHub logins. Omit clocks \
                      and changing ages; emit nothing for no items and propagate read failures. \
                      Empty/unchanged checks post nothing. `list` shows pending wakes; `cancel` takes an `id`. \
                      In a mapped channel, `listSubscriptions` lists this channel's PR subscriptions; \
                      `cancelSubscription` cancels one by `id`."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["schedule", "watch", "digest", "list", "cancel", "listSubscriptions", "cancelSubscription"] },
                "note": { "type": "string", "description": "What to do when you wake." },
                "afterSeconds": { "type": "integer", "minimum": 1 },
                "script": { "type": "string" },
                "everySeconds": { "type": "integer", "minimum": 1 },
                "forSeconds": { "type": "integer", "minimum": 1 },
                "id": { "type": "integer" }
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
) -> Result<crate::memory::Outcome, PromptError> {
    let request = match serde_json::from_str::<WakeCall>(&call.function.arguments) {
        Ok(request) => request,
        Err(source) => {
            let error = PromptError::InvalidWake {
                tool: call.function.name.clone(),
                source,
            };
            reject_tool_call(model_turn, call.id.as_str(), error.telemetry_kind(), false);
            return Err(error);
        }
    };
    let result = match request {
        WakeCall::Digest { note, script, every_seconds, for_seconds } => registrar
            .schedule(WakeRequest::Digest {
                note, script,
                every: Duration::from_secs(every_seconds),
                until: Duration::from_secs(for_seconds),
            })
            .map(|summary| format!("Daily digest registered. It expires in {} seconds; request it again after expiry or cancellation to renew. Cancel with wake action cancel and id {}.\n{}", summary.watch.as_ref().map_or(0, |watch| watch.ends_in.as_secs()), summary.id, render(&summary))),
        WakeCall::ListSubscriptions {} => registrar.list_subscriptions().map(|summaries| {
            if summaries.is_empty() {
                "No PR subscriptions.".to_owned()
            } else {
                summaries
                    .iter()
                    .map(|summary| {
                        format!(
                            "subscription {}: expires in {} seconds\n{}",
                            summary.id,
                            summary.due_in.as_secs(),
                            summary.note
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
        }),
        WakeCall::CancelSubscription { id } => registrar
            .cancel_subscription(WakeId(id))
            .map(|()| format!("Cancelled subscription {id}.")),
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
    let outcome = if result.is_ok() {
        crate::memory::Outcome::Exit(0)
    } else {
        crate::memory::Outcome::Refused
    };
    let text = match result {
        Ok(text) => text,
        Err(refusal) => {
            tracing::info!(
                target: "dekopon_agent::audit",
                {
                    audit.event = "agent.wake.refused",
                    model.turn = model_turn,
                    gen_ai.tool.call.id = dekopon_core::bounded_attribute(call.id.as_str()).as_ref(),
                    reason = refusal.telemetry_kind(),
                },
                "agent.wake.refused"
            );
            format!("Not done: {refusal}.")
        }
    };
    messages.push(ModelMessage::tool(call.id.clone(), text));
    Ok(outcome)
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
