//! Configured model intent and checkpointed safe-boundary application. Tools confer no authority.
use std::sync::{Arc, Mutex};

use dekopon_broker_protocol::{
    ClientErrorKind, ControlClient, ControlOutcome, ControlScope, ControlTarget,
    MAX_CONTROL_ATTEMPTS, VerifiedControlDecision, validate_control_targets,
};
use dekopon_core::{ConfiguredModelId, Effort, InvocationId, ModelSelection, SurfaceEpoch};
use dekopon_model::model::{ChatModel, CompletionOptions, ModelTool, ModelToolCall};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

pub const SELECT_MODEL_TOOL: &str = "select_model";
pub const SET_EFFORT_TOOL: &str = "set_effort";

/// Immutable, credential-free identity for transition/accounting linkage, not provider authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelIdentity {
    pub configured: Option<ConfiguredModelId>,
    pub backend: String,
    pub model: String,
    pub effort: Effort,
}
impl ModelIdentity {
    pub fn selection(&self) -> Option<ModelSelection> {
        self.configured.clone().map(|model| ModelSelection {
            model,
            effort: self.effort,
        })
    }
}

/// A host-owned cached client. Preparing one must perform no inference or external effect.
pub struct PreparedModel {
    pub identity: ModelIdentity,
    pub client: Arc<dyn ChatModel + Send + Sync>,
    pub accepts_images: bool,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PreparationError {
    #[error("model is not a configured route candidate")]
    UnknownModel,
    #[error("model adapter cannot encode the requested effort")]
    UnsupportedEffort,
    #[error("configured model client could not be prepared")]
    Unavailable,
    #[error("prepared model identity did not match the configured request")]
    InvalidIdentity,
}

/// Narrow host seam: only configured candidates, no endpoint/credential input or policy engine.
pub trait ModelRegistry {
    fn candidates(&self) -> Vec<ControlTarget>;
    fn prepare(&self, selection: &ModelSelection) -> Result<PreparedModel, PreparationError>;
}

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("invalid configured control surface")]
    Configuration,
    /// The session's configured control surface is unusable, naming every conflict at once.
    ///
    /// Separate from [`Self::Configuration`], which is a live client failure rather than an
    /// authored one: an operator reading "invalid configured control surface" learned nothing about
    /// which of three independent checks refused, and there was no event carrying it either.
    #[error("invalid configured control surface: {reason}")]
    Surface {
        /// Every conflict, joined, in the order they are checked.
        reason: String,
    },
    #[error(transparent)]
    Preparation(#[from] PreparationError),
    #[error("control exchange failed; no retry or further inference is safe: {0}")]
    Authorization(#[from] dekopon_broker_protocol::ClientError),
}

/// Why a control transition could not be authorized, on the axis an operator acts on.
///
/// Carried by [`TransitionOutcome::AuthorizationFailed`] so it survives into the checkpointed
/// transition record and the accounting event. `AuthorizationFailed` on its own collapses a
/// substituted decision binding, a broker that never answered, and a spent attempt budget into one
/// token, and the underlying `ClientError` was logged nowhere at all.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlFailureKind {
    /// The transition was interrupted before any broker answer — a checkpoint or host failure.
    Interrupted,
    /// This session's own control surface or client was unusable.
    Configuration,
    /// A configured model could not be prepared.
    Preparation,
    /// The broker exchange failed, carrying the client's own stable kind.
    Client(ClientErrorKind),
}

impl ControlFailureKind {
    /// Classifies one control failure without discarding which client failure produced it.
    #[must_use]
    pub fn of(error: &ControlError) -> Self {
        match error {
            ControlError::Configuration | ControlError::Surface { .. } => Self::Configuration,
            ControlError::Preparation(_) => Self::Preparation,
            ControlError::Authorization(error) => Self::Client(error.kind()),
        }
    }

    /// The stable token for this kind, as telemetry spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "interrupted",
            Self::Configuration => "configuration",
            Self::Preparation => "preparation",
            Self::Client(kind) => kind.as_str(),
        }
    }
}

impl std::fmt::Display for ControlFailureKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A request-bound live authenticated authorizer plus configured registry. Never serialized.
/// Direct/replay runners must not install this merely because they have a provider broker leg.
pub struct SessionControls<'a> {
    registry: &'a dyn ModelRegistry,
    baseline: ModelSelection,
    candidates: Vec<ControlTarget>,
    scope: ControlScope,
    epoch: SurfaceEpoch,
    client: Mutex<ControlClient>,
    executor: tokio::runtime::Handle,
    max_attempts: u32,
}
impl<'a> SessionControls<'a> {
    pub fn new(
        registry: &'a dyn ModelRegistry,
        baseline: ModelSelection,
        client: ControlClient,
        executor: tokio::runtime::Handle,
        max_attempts: u32,
    ) -> Result<Self, ControlError> {
        let mut candidates = registry.candidates();
        // Every conflict, then fail. Three independent checks used to share one silent refusal, so
        // an operator whose attempt budget *and* baseline were wrong fixed one, restarted, and
        // learned about the other.
        let mut conflicts = Vec::new();
        if let Err(error) = validate_control_targets(&candidates) {
            conflicts.push(format!("configured control targets are invalid: {error}"));
        }
        if !(1..=MAX_CONTROL_ATTEMPTS).contains(&max_attempts) {
            conflicts.push(format!(
                "control attempt budget {max_attempts} is outside 1..={MAX_CONTROL_ATTEMPTS}"
            ));
        }
        if !candidates
            .iter()
            .any(|c| c.model == baseline.model && c.efforts.contains(&baseline.effort))
        {
            conflicts.push(format!(
                "baseline selection {}/{} is not a configured control target",
                baseline.model, baseline.effort
            ));
        }
        if !conflicts.is_empty() {
            let reason = conflicts.join("; ");
            tracing::error!(cause_type = "control-surface", cause = %reason);
            return Err(ControlError::Surface { reason });
        }
        candidates.sort_by(|a, b| a.model.cmp(&b.model));
        for candidate in &mut candidates {
            candidate.efforts.sort();
        }
        Ok(Self {
            registry,
            baseline,
            candidates,
            scope: client.scope().clone(),
            epoch: client.surface_epoch().clone(),
            client: Mutex::new(client),
            executor,
            max_attempts,
        })
    }
    pub(crate) fn scope(&self) -> &ControlScope {
        &self.scope
    }
    pub(crate) fn job(&self) -> &str {
        self.scope.job.as_str()
    }
    pub(crate) fn epoch(&self) -> &SurfaceEpoch {
        &self.epoch
    }
    pub(crate) fn baseline(&self) -> &ModelSelection {
        &self.baseline
    }
    pub(crate) fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    pub(crate) fn prepare(
        &self,
        selection: &ModelSelection,
    ) -> Result<PreparedModel, PreparationError> {
        let target = self
            .candidates
            .iter()
            .find(|c| c.model == selection.model)
            .ok_or(PreparationError::UnknownModel)?;
        if !target.efforts.contains(&selection.effort) {
            return Err(PreparationError::UnsupportedEffort);
        }
        let prepared = self.registry.prepare(selection)?;
        if prepared.identity.selection().as_ref() != Some(selection)
            || prepared.identity.model.is_empty()
            || prepared.identity.model.len() > 256
            || prepared.identity.backend.is_empty()
            || prepared.identity.backend.len() > 64
            || prepared.identity.model.chars().any(char::is_control)
            || prepared.identity.backend.chars().any(char::is_control)
        {
            return Err(PreparationError::InvalidIdentity);
        }
        if !prepared.client.supports_effort(selection.effort) {
            return Err(PreparationError::UnsupportedEffort);
        }
        Ok(prepared)
    }

    pub(crate) fn authorize(
        &self,
        sequence: u32,
        id: InvocationId,
        from: ModelSelection,
        to: ModelSelection,
    ) -> Result<VerifiedControlDecision, ControlError> {
        let mut client = self.client.lock().map_err(|error| {
            tracing::error!(cause_type = "control-client-lock", %error);
            ControlError::Configuration
        })?;
        // Called only on the host's blocking executor; bounded exchange is drained even on Stop.
        Ok(self.executor.block_on(
            client.authorize(
                sequence,
                id,
                from,
                to,
                self.scope
                    .job
                    .as_str()
                    .parse()
                    .expect("validated opaque job is a trace ID"),
                crate::runtime::current_trace_parent(),
            ),
        )?)
    }

    pub(crate) fn tools(
        &self,
        current: &ModelIdentity,
        client: &dyn ChatModel,
        spent: u32,
    ) -> Vec<ModelTool> {
        if spent >= self.max_attempts {
            return Vec::new();
        }
        let mut tools = Vec::new();
        if self
            .candidates
            .iter()
            .any(|c| Some(&c.model) != current.configured.as_ref())
        {
            // Only efforts some candidate actually carries. Offering all four mirrored nothing:
            // the broker requires both `from` and `to` to appear in `controlTargets`, so an effort
            // no candidate lists makes every proposal naming it `target-denied` while still costing
            // prompt tokens and one of the job's four attempts.
            let efforts = self
                .candidates
                .iter()
                .flat_map(|c| c.efforts.iter())
                .collect::<std::collections::BTreeSet<_>>();
            tools.push(ModelTool {
                name: SELECT_MODEL_TOOL.into(),
                description: "Request a configured model, never an endpoint. Candidates are not grants: the broker authorizes every change. Must be the sole tool in a turn. Omitted effort preserves the current effort; same-target requests are refused without switching.".into(),
                parameters: json!({"type":"object","additionalProperties":false,"required":["model"],"properties":{
                    "model":{"type":"string","enum":self.candidates.iter().map(|c| c.model.as_str()).collect::<Vec<_>>()},
                    "effort":{"type":"string","enum":efforts}
                }}),
            });
        }
        if let Some(candidate) = self
            .candidates
            .iter()
            .find(|c| Some(&c.model) == current.configured.as_ref())
            && candidate
                .efforts
                .iter()
                .any(|effort| *effort != current.effort && client.supports_effort(*effort))
        {
            tools.push(ModelTool {
                name: SET_EFFORT_TOOL.into(),
                description: "Request inference effort for the current model. providerDefault omits the setting; it is not an explicit level. Fresh broker authorization is required. Must be the sole tool in a turn; no-ops are refused.".into(),
                parameters: json!({"type":"object","additionalProperties":false,"required":["effort"],"properties":{
                    "effort":{"type":"string","enum":candidate.efforts.iter().filter(|e| client.supports_effort(**e)).collect::<Vec<_>>()}
                }}),
            });
        }
        tools
    }
}

/// Every proposed transition, including local refusals, has a factual immutable record. No tokens
/// or prices are guessed here; the accounting owner attaches tracker snapshots at these boundaries.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TransitionRecord {
    pub sequence: u32,
    pub requesting_call: Option<u32>,
    pub attempt: Option<u32>,
    pub control_id: Option<InvocationId>,
    pub from: ModelIdentity,
    /// None explicitly means an invalid/unresolved target, never the old target by default.
    pub requested: Option<ModelSelection>,
    pub to: Option<ModelIdentity>,
    pub outcome: TransitionOutcome,
    pub decision_ref: Option<String>,
    pub context_revision: u64,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransitionOutcome {
    Pending,
    Applied,
    Denied,
    InvalidArguments,
    Disabled,
    NoOp,
    UnknownModel,
    UnsupportedEffort,
    BatchRefused,
    AttemptsExhausted,
    PreparationFailed,
    /// No admission was obtained, naming why. The cause travels to the accounting event and the
    /// embedder's failure record rather than stopping at the tracing call that discarded it.
    AuthorizationFailed {
        cause: ControlFailureKind,
    },
    Cancelled,
    IncompatibleAssets,
}
impl TransitionOutcome {
    pub(crate) fn result(self) -> &'static str {
        match self {
            Self::Applied => {
                "Configured selection applied. Context was rebuilt; all consumed budgets and execution evidence are preserved."
            }
            Self::InvalidArguments => {
                "Invalid control arguments. Use only a configured model and providerDefault, low, medium, or high effort."
            }
            Self::Denied => "Control denied. The previous selection remains active.",
            Self::NoOp => "No change: that model and effort are already selected. Attempt spent.",
            Self::BatchRefused => {
                "No tools in this batch ran. A model/effort control must be the sole tool in a turn."
            }
            Self::UnsupportedEffort => {
                "Unsupported effort for the configured model. No change applied."
            }
            Self::AttemptsExhausted => "Control attempt limit reached. No change applied.",
            Self::Disabled => "Model/effort controls are unavailable in this session.",
            Self::UnknownModel => "Target is not a configured route candidate. No change applied.",
            Self::IncompatibleAssets => {
                "Cannot change input modalities while request-local attachments are available. No change applied."
            }
            _ => "Control not applied. No automatic retry is safe after an interrupted transition.",
        }
    }
}
pub(crate) fn is_control(call: &ModelToolCall) -> bool {
    matches!(
        call.function.name.as_str(),
        SELECT_MODEL_TOOL | SET_EFFORT_TOOL
    )
}
pub(crate) fn parse(
    call: &ModelToolCall,
    from: &ModelIdentity,
) -> Result<ModelSelection, TransitionOutcome> {
    fn present_effort<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Effort>, D::Error> {
        Effort::deserialize(d).map(Some)
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Select {
        model: ConfiguredModelId,
        #[serde(default, deserialize_with = "present_effort")]
        effort: Option<Effort>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Set {
        effort: Effort,
    }
    if call.kind != "function" || call.function.arguments.len() > 1024 {
        return Err(TransitionOutcome::InvalidArguments);
    }
    match call.function.name.as_str() {
        SELECT_MODEL_TOOL => serde_json::from_str::<Select>(&call.function.arguments)
            .map(|s| ModelSelection {
                model: s.model,
                effort: s.effort.unwrap_or(from.effort),
            })
            .map_err(|error| {
                tracing::debug!(cause_type = "control-arguments", category = ?error.classify());
                TransitionOutcome::InvalidArguments
            }),
        SET_EFFORT_TOOL => serde_json::from_str::<Set>(&call.function.arguments)
            .map_err(|error| {
                tracing::debug!(cause_type = "control-arguments", category = ?error.classify());
                TransitionOutcome::InvalidArguments
            })
            .and_then(|s| {
                from.configured
                    .clone()
                    .map(|model| ModelSelection {
                        model,
                        effort: s.effort,
                    })
                    .ok_or(TransitionOutcome::Disabled)
            }),
        _ => Err(TransitionOutcome::InvalidArguments),
    }
}

pub(crate) struct ActiveModel {
    pub prepared: Option<PreparedModel>,
    pub identity: ModelIdentity,
    pub options: CompletionOptions,
}

pub(crate) fn consume(decision: VerifiedControlDecision, record: &mut TransitionRecord) -> bool {
    record.decision_ref = Some(decision.decision_ref().to_owned());
    decision.consume() == ControlOutcome::Admitted
}

mod transition;
pub(crate) use transition::{TransitionRequest, save_boundary, transition};

#[cfg(test)]
mod tests;
