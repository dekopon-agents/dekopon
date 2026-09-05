use super::*;
use crate::{
    checkpoint::{CheckpointError, ExecutionJournal, Position},
    session::{CancellationProbe, PromptError, SessionState},
};

pub(crate) struct TransitionRequest {
    pub selection: Result<ModelSelection, TransitionOutcome>,
    pub refusal: Option<TransitionOutcome>,
    pub requesting_call: Option<u32>,
    pub assets_present: bool,
}

pub(crate) fn transition(
    controls: Option<&SessionControls<'_>>,
    mut request: TransitionRequest,
    active: &mut ActiveModel,
    state: &mut SessionState,
    journal: &ExecutionJournal<'_>,
    cancellation: Option<&dyn CancellationProbe>,
) -> Result<TransitionOutcome, PromptError> {
    let tracker = journal.accounting.snapshot();
    if let Some(turn) = request.requesting_call {
        request.requesting_call = tracker
            .calls
            .iter()
            .find(|c| c.kind == crate::accounting::CallKind::Chat && c.model_turn == turn)
            .map(|c| c.sequence);
    }
    let before = tracker.totals();
    let started = std::time::Instant::now();
    let prior = state.transitions.len();
    let result = transition_inner(controls, request, active, state, journal, cancellation);
    if state.transitions.len() > prior {
        let record = state.transitions.last_mut().expect("reserved transition");
        if result.is_err() && record.outcome == TransitionOutcome::Pending {
            // Still pending after an error means the transition never reached the broker's answer:
            // a checkpoint write or the host itself failed underneath it.
            record.outcome = TransitionOutcome::AuthorizationFailed {
                cause: ControlFailureKind::Interrupted,
            };
        }
        journal
            .accounting
            .transition(record, before, started.elapsed());
    }
    result
}

fn transition_inner(
    controls: Option<&SessionControls<'_>>,
    request: TransitionRequest,
    active: &mut ActiveModel,
    state: &mut SessionState,
    journal: &ExecutionJournal<'_>,
    cancellation: Option<&dyn CancellationProbe>,
) -> Result<TransitionOutcome, PromptError> {
    let snapshot = journal.snapshot();
    if snapshot.pending_execution.is_some()
        || snapshot.record.has_unknown_work()
        || snapshot.history.has_unknown_work()
    {
        return Err(CheckpointError::UnknownWork.into());
    }
    if state.transitions.len() >= 128 * crate::tools::MAX_TOOL_CALLS_PER_TURN {
        return Err(CheckpointError::Capacity.into());
    }
    let attempt = if state.spent.control_attempts
        < controls.map_or(MAX_CONTROL_ATTEMPTS, SessionControls::max_attempts)
    {
        state.spent.control_attempts += 1;
        Some(state.spent.control_attempts)
    } else {
        None
    };
    let mut outcome = if controls.is_none() {
        TransitionOutcome::Disabled
    } else if attempt.is_none() {
        TransitionOutcome::AttemptsExhausted
    } else if let Some(refusal) = request.refusal {
        refusal
    } else if let Err(refusal) = request.selection {
        refusal
    } else {
        TransitionOutcome::Pending
    };
    let requested = request.selection.ok();
    if outcome == TransitionOutcome::Pending && requested == active.identity.selection() {
        outcome = TransitionOutcome::NoOp;
    }
    state.transitions.push(TransitionRecord {
        sequence: state.transitions.len() as u32 + 1,
        requesting_call: request.requesting_call,
        attempt,
        control_id: None,
        from: active.identity.clone(),
        requested: requested.clone(),
        to: (outcome == TransitionOutcome::NoOp).then(|| active.identity.clone()),
        outcome,
        decision_ref: None,
        context_revision: snapshot.context_revision,
    });
    // The attempt and intent are checkpointed before client preparation or broker transmission.
    journal.update(|c| {
        c.state = state.clone();
        c.position = Position::ControlPending;
    })?;
    if outcome != TransitionOutcome::Pending {
        return Ok(outcome);
    }
    let controls = controls.expect("pending controls have an authorizer");
    let requested = requested.expect("pending selection parsed");
    let prepared = match controls.prepare(&requested) {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::warn!(cause_type = "control-preparation", %error);
            let outcome = match error {
                PreparationError::UnknownModel => TransitionOutcome::UnknownModel,
                PreparationError::UnsupportedEffort => TransitionOutcome::UnsupportedEffort,
                _ => TransitionOutcome::PreparationFailed,
            };
            state
                .transitions
                .last_mut()
                .expect("reserved transition")
                .outcome = outcome;
            return Ok(outcome);
        }
    };
    let record = state.transitions.last_mut().expect("reserved transition");
    record.to = Some(prepared.identity.clone());
    if request.assets_present
        && active
            .prepared
            .as_ref()
            .is_some_and(|p| p.accepts_images != prepared.accepts_images)
    {
        record.outcome = TransitionOutcome::IncompatibleAssets;
        return Ok(record.outcome);
    }
    if cancellation.is_some_and(CancellationProbe::is_cancelled) {
        record.outcome = TransitionOutcome::Cancelled;
        state.control_fenced = true;
        return Err(PromptError::Cancelled);
    }
    let id: InvocationId = crate::checkpoint::opaque_id()
        .parse()
        .expect("opaque control ID");
    record.control_id = Some(id.clone());
    journal.update(|c| c.state = state.clone())?;
    let decision = controls.authorize(
        attempt.expect("pending attempt"),
        id,
        active
            .identity
            .selection()
            .expect("configured active model"),
        requested,
    );
    let record = state.transitions.last_mut().expect("reserved transition");
    let decision = match decision {
        Ok(decision) => decision,
        Err(error) => {
            // The kind, not just the category. `control-authorization` alone made a substituted
            // decision binding — the one failure that says something answered the socket and lied —
            // indistinguishable from a broker that was simply not running.
            let cause = ControlFailureKind::of(&error);
            tracing::error!(
                cause_type = "control-authorization",
                cause = %cause,
                error = %dekopon_core::error_chain(&error),
            );
            record.outcome = TransitionOutcome::AuthorizationFailed { cause };
            state.control_fenced = true;
            return Err(error.into());
        }
    };
    let admitted = consume(decision, record);
    if cancellation.is_some_and(CancellationProbe::is_cancelled) {
        record.outcome = TransitionOutcome::Cancelled;
        state.control_fenced = true;
        return Err(PromptError::Cancelled);
    }
    if !admitted {
        record.outcome = TransitionOutcome::Denied;
        return Ok(record.outcome);
    }

    // No model call here. Opaque continuation/context is replaced by the caller before its
    // post-transition checkpoint. Neither evidence nor any spent limit or one-attempt flag resets.
    active.identity = prepared.identity.clone();
    active.options = active
        .options
        .clone()
        .with_effort(active.identity.effort)
        .with_prompt_cache_key(crate::checkpoint::opaque_id());
    active.prepared = Some(prepared);
    state.current_model = Some(active.identity.clone());
    state.agent_config_shown = false;
    state.skill_reads = Default::default();
    record.outcome = TransitionOutcome::Applied;
    record.context_revision = snapshot
        .context_revision
        .checked_add(1)
        .ok_or(CheckpointError::Capacity)?;
    Ok(record.outcome)
}

pub(crate) fn save_boundary(
    state: &SessionState,
    journal: &ExecutionJournal<'_>,
    messages: &[dekopon_model::model::ModelMessage],
) -> Result<(), CheckpointError> {
    journal.update(|c| {
        c.state = state.clone();
        c.position = Position::Tools;
        if let Some(model) = &state.current_model {
            c.model = model.model.clone();
            c.effort = model.effort.to_string();
        }
        if let Some(record) = state.transitions.last() {
            c.context_revision = record.context_revision;
        }
        if let Some(group) = c.record.groups.last_mut() {
            group.capture_results(messages);
        }
    })
}
