//! The policy is the sole terminal writer once a session starts, so a stopped reply can't land
//! ahead of the partial answer it follows.

use std::time::Duration;

use dekopon_agent::{BudgetLimit, CancelSource, CancelVia};
use dekopon_model::error::InferenceErrorKind;

use crate::session::{
    EMPTY_ANSWER_REPLY, MAX_STEPS_REPLY, MODEL_DEADLINE_REPLY, SESSION_TASK_REPLY, WALL_CLOCK_REPLY,
};

mod adapter;
mod policy;
mod text;

#[cfg(test)]
mod tests;

pub(crate) use policy::{ProgressInputs, ProgressPolicy, Terminal, bounded};
pub(crate) use text::{ProgressDetail, ProgressText, Templates};

pub(crate) const DEFAULT_KEEP_ALIVE_AT: [u64; 2] = [15, 45];
pub(crate) const DEFAULT_KEEP_ALIVE_EVERY: u64 = 60;
pub(crate) const DEFAULT_KEEP_ALIVE_MAX: u32 = 10;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KeepAlive {
    pub at: Vec<Duration>,
    pub every: Duration,
    pub max: u32,
}

impl Default for KeepAlive {
    fn default() -> Self {
        Self {
            at: DEFAULT_KEEP_ALIVE_AT
                .iter()
                .map(|seconds| Duration::from_secs(*seconds))
                .collect(),
            every: Duration::from_secs(DEFAULT_KEEP_ALIVE_EVERY),
            max: DEFAULT_KEEP_ALIVE_MAX,
        }
    }
}

pub(crate) const fn cancel_label(source: CancelSource) -> &'static str {
    match source {
        CancelSource::User {
            via: CancelVia::NativeStop,
        } => "user:native-stop",
        CancelSource::User {
            via: CancelVia::Button,
        } => "user:button",
        CancelSource::User {
            via: CancelVia::StopReply,
        } => "user:stop-reply",
        CancelSource::Operator => "operator",
        CancelSource::Budget {
            limit: BudgetLimit::WallClock,
        } => "budget:wall-clock",
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum StopCause {
    Cancelled(CancelSource),
    Model(InferenceErrorKind),
    EmptyAnswer,
    MaxSteps,
    SessionTask,
}

impl StopCause {
    pub(crate) fn line(self, templates: &Templates) -> &str {
        match self {
            Self::Cancelled(CancelSource::User { .. } | CancelSource::Operator) => {
                templates.stopped()
            }
            Self::Cancelled(CancelSource::Budget {
                limit: BudgetLimit::WallClock,
            }) => WALL_CLOCK_REPLY,
            Self::Model(InferenceErrorKind::DeadlineExceeded) => MODEL_DEADLINE_REPLY,
            Self::Model(
                InferenceErrorKind::InvalidRequest
                | InferenceErrorKind::Provider
                | InferenceErrorKind::Transport
                | InferenceErrorKind::Protocol
                | InferenceErrorKind::Attachment
                | InferenceErrorKind::Authentication
                | InferenceErrorKind::RateLimited
                | InferenceErrorKind::Cancelled
                | InferenceErrorKind::OverBudget,
            ) => templates.failed(),
            Self::EmptyAnswer => EMPTY_ANSWER_REPLY,
            Self::MaxSteps => MAX_STEPS_REPLY,
            Self::SessionTask => SESSION_TASK_REPLY,
        }
    }
}

impl StopCause {
    pub(crate) fn notice(self) -> String {
        let reason = match self {
            Self::Cancelled(CancelSource::User { .. }) => "the person stopped it".to_owned(),
            Self::Cancelled(CancelSource::Operator) => "the gateway stopped it".to_owned(),
            Self::Cancelled(CancelSource::Budget {
                limit: BudgetLimit::WallClock,
            }) => "it reached its time limit".to_owned(),
            Self::Model(kind) => format!("the model call failed ({})", kind.as_str()),
            Self::EmptyAnswer => "the model returned an empty answer".to_owned(),
            Self::MaxSteps => "it reached its step limit".to_owned(),
            Self::SessionTask => "the gateway lost its task".to_owned(),
        };
        format!(
            "[gateway: the previous turn stopped before answering: {reason}. Capability calls already made were not undone.]"
        )
    }
}
