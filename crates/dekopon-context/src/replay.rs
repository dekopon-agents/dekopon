use crate::{
    EnvelopeSource, Exchange, Representation, fit_projected, original_envelope_bytes,
    render_envelope,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReplayRecord {
    pub id: String,
    pub user: Option<String>,
    pub answer: Option<String>,
    #[serde(default)]
    pub representations: Vec<Representation>,
    #[serde(default)]
    pub exchanges: Vec<Exchange>,
    pub pin: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReplayRequest {
    pub version: String,
    pub config_id: String,
    pub ordinal: usize,
    pub input: ReplayRecord,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ScriptedChoice {
    pub id: String,
    pub version: String,
    pub is_original: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CassetteEvent {
    pub request: ReplayRequest,
    pub response: ScriptedChoice,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReplayInput {
    pub version: String,
    pub config_id: String,
    pub max_turns: usize,
    pub max_bytes: usize,
    pub steps: Vec<ReplayRecord>,
    pub cassette: Vec<CassetteEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct VisibleTurn {
    pub record_id: String,
    pub user: String,
    pub answer: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Snapshot {
    pub retained: Vec<ReplayRecord>,
    pub model_visible: Vec<VisibleTurn>,
    pub omitted_record_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayError {
    InvalidInput,
    MissingEvent,
    MismatchedEvent,
    ExtraEvent,
}

pub fn run(input: &ReplayInput) -> Result<Vec<Snapshot>, ReplayError> {
    if input.version.is_empty()
        || input.config_id.is_empty()
        || input.max_turns == 0
        || input.max_turns > 32
        || input.max_bytes > 65_536
        || input.steps.len() > 32
        || input.steps.iter().any(|step| {
            step.id.is_empty()
                || !step.id.bytes().all(|b| b.is_ascii_digit())
                || serde_json::to_vec(step).map_or(true, |encoded| encoded.len() > 4096)
        })
    {
        return Err(ReplayError::InvalidInput);
    }
    let mut retained = Vec::new();
    let mut visible: Vec<VisibleTurn> = Vec::new();
    let mut omitted = Vec::new();
    let mut snapshots = Vec::new();
    for (ordinal, step) in input.steps.iter().enumerate() {
        let expected = ReplayRequest {
            version: input.version.clone(),
            config_id: input.config_id.clone(),
            ordinal,
            input: step.clone(),
        };
        let event = input
            .cassette
            .get(ordinal)
            .ok_or(ReplayError::MissingEvent)?;
        if event.request != expected {
            return Err(ReplayError::MismatchedEvent);
        }
        retained.push(step.clone());
        let choice = &event.response;
        let rendered = render_envelope(
            EnvelopeSource {
                user: step.user.as_deref(),
                answer: step.answer.as_deref(),
                representations: &step.representations,
                exchanges: &step.exchanges,
            },
            &choice.id,
            &choice.version,
            choice.is_original,
        );
        if let Some(text) = rendered {
            let split_original =
                choice.is_original && step.exchanges.is_empty() && step.user.is_some();
            let bytes = if split_original {
                original_envelope_bytes(
                    step.user.as_deref().unwrap_or_default(),
                    step.answer.as_deref(),
                    &[],
                )
            } else {
                text.len()
            };
            if let Some(drop_count) = fit_projected(
                visible
                    .iter()
                    .map(|turn| turn.user.len() + turn.answer.as_ref().map_or(0, String::len)),
                bytes,
                input.max_turns,
                input.max_bytes,
            ) {
                visible.drain(..drop_count);
                let (user, answer) = if split_original {
                    (step.user.clone().unwrap_or_default(), step.answer.clone())
                } else {
                    (text, None)
                };
                visible.push(VisibleTurn {
                    record_id: step.id.clone(),
                    user,
                    answer,
                });
            } else {
                omitted.push(step.id.clone());
            }
        } else {
            omitted.push(step.id.clone());
        }
        snapshots.push(Snapshot {
            retained: retained.clone(),
            model_visible: visible.clone(),
            omitted_record_ids: omitted.clone(),
        });
    }
    if input.cassette.len() != input.steps.len() {
        return Err(ReplayError::ExtraEvent);
    }
    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatch_refuses_without_consuming_another_response() {
        let step = ReplayRecord {
            id: "9007199254740993".into(),
            user: Some("a".into()),
            answer: None,
            representations: vec![],
            exchanges: vec![],
            pin: None,
        };
        let mut input = ReplayInput {
            version: "v1".into(),
            config_id: "synthetic".into(),
            max_turns: 2,
            max_bytes: 4,
            steps: vec![step.clone()],
            cassette: vec![],
        };
        assert_eq!(run(&input), Err(ReplayError::MissingEvent));
        input.cassette.push(CassetteEvent {
            request: ReplayRequest {
                version: "v1".into(),
                config_id: "other".into(),
                ordinal: 0,
                input: step,
            },
            response: ScriptedChoice {
                id: "original".into(),
                version: "v1".into(),
                is_original: true,
            },
        });
        assert_eq!(run(&input), Err(ReplayError::MismatchedEvent));
    }
}
