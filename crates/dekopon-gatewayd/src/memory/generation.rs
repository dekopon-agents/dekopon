use super::journal;
use crate::{
    asset::AssetAccess,
    config::MemoryWindow,
    conversation::ConversationKey,
    routes::BoundRoute,
    session::{SessionCancellation, SessionRunner},
};
use dekopon_agent::prompt::{ConversationTurn, Exchange, Representation};
use dekopon_model::model::{ChatModel, CompletionOptions, ModelMessage};
use sha2::{Digest, Sha256};
use std::{ops::ControlFlow, sync::Arc, time::SystemTime};
use tracing::Instrument as _;

use crate::config::memory::{ResolvedPolicy, Routine, RoutineKind};

pub(crate) struct Prepared {
    pub include_chat: bool,
    pub representations: Vec<Representation>,
    pub exchanges: Vec<Exchange>,
    pub original: bool,
    pub availability: Vec<String>,
    pub failed_routines: Vec<String>,
}

pub(crate) fn version(routine: &Routine) -> String {
    let bytes = serde_json::to_vec(routine).expect("validated routine serializes");
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn captured_bytes(turn: &ConversationTurn, include_chat: bool) -> u64 {
    let chat = if include_chat { turn.bytes() as u64 } else { 0 };
    turn.exchanges()
        .iter()
        .flat_map(|exchange| exchange.request.iter().chain(&exchange.result))
        .map(|rep| rep.captured_bytes)
        .fold(chat, u64::saturating_add)
}

fn bounded(text: &str, bytes: usize) -> String {
    text[..text.floor_char_boundary(bytes.min(text.len()))].to_owned()
}

fn derived(
    source: &Representation,
    routine: &Routine,
    text: Option<String>,
    part: Option<&str>,
) -> Representation {
    Representation {
        routine_id: routine.id.clone(),
        version: version(routine),
        source_record_id: source.source_record_id,
        part: part.map(str::to_owned),
        kind: match routine.kind {
            RoutineKind::Original => "original",
            RoutineKind::Prefix => "prefix",
            RoutineKind::ProgressiveOneShot => "progressiveOneShot",
            RoutineKind::ProgressiveTurducken => "progressiveTurducken",
        }
        .to_owned(),
        captured_bytes: source.captured_bytes,
        text,
    }
}

fn project(
    source: &[Representation],
    routines: &[&Routine],
    retained_original: Option<&Routine>,
) -> Vec<Representation> {
    source
        .iter()
        .flat_map(|item| {
            let mut projected = Vec::new();
            let mut metadata = item.clone();
            if let Some(routine) = retained_original {
                metadata.routine_id.clone_from(&routine.id);
                metadata.version = version(routine);
            } else {
                metadata.text = None;
            }
            projected.push(metadata);
            for routine in routines {
                if routine.kind == RoutineKind::Prefix {
                    projected.push(derived(
                        item,
                        routine,
                        item.text.as_deref().map(|text| {
                            bounded(
                                text,
                                routine.max_bytes.map_or(0, |size| size.get() as usize),
                            )
                        }),
                        None,
                    ));
                }
            }
            projected
        })
        .collect()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PartResponse {
    id: String,
    text: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OneShotResponse {
    parts: Vec<PartResponse>,
}

fn complete(
    model: &dyn ChatModel,
    source: &str,
    earlier: &str,
    instructions: &str,
) -> Option<String> {
    let turn = model
        .complete(
            &[
                ModelMessage::system(instructions),
                ModelMessage::user(format!("Source:\n{source}\nEarlier parts:\n{earlier}")),
            ],
            &[],
            &CompletionOptions::default(),
            &mut |_event| ControlFlow::Continue(()),
        )
        .ok()?;
    turn.content
        .filter(|text| turn.tool_calls.is_empty() && !text.is_empty())
}

pub(crate) fn prepare(
    turn: &ConversationTurn,
    policy: &ResolvedPolicy,
    mut model_for: impl FnMut(&str) -> Option<crate::session::SharedModel>,
    include_chat: bool,
) -> Prepared {
    let routines: Vec<_> = policy
        .routines
        .iter()
        .filter(|routine| policy.retain.contains(&routine.id))
        .collect();
    let retained_original = routines
        .iter()
        .copied()
        .find(|routine| routine.kind == RoutineKind::Original);
    let original = retained_original.is_some();
    let mut availability = Vec::new();
    let mut failed_routines = Vec::new();
    let mut representations = Vec::new();
    let mut exchanges = turn.exchanges().to_vec();
    for exchange in &mut exchanges {
        exchange.request = project(&exchange.request, &routines, retained_original);
        exchange.result = project(&exchange.result, &routines, retained_original);
    }
    let source = if include_chat {
        serde_json::json!({
            "user": turn.user(), "answer": turn.answer(), "exchanges": turn.exchanges(),
        })
    } else {
        serde_json::json!({"exchanges": turn.exchanges()})
    }
    .to_string();
    let source_rep = Representation::original(&source, "");
    let mut metadata = source_rep.clone();
    metadata.text = None;
    metadata.captured_bytes = captured_bytes(turn, include_chat);
    if let Some(routine) = retained_original {
        metadata.routine_id.clone_from(&routine.id);
        metadata.version = version(routine);
    }
    representations.push(metadata);
    for routine in routines {
        match routine.kind {
            RoutineKind::Original => availability.push(routine.id.clone()),
            RoutineKind::Prefix => {
                representations.push(derived(
                    &source_rep,
                    routine,
                    Some(bounded(
                        &source,
                        routine.max_bytes.map_or(0, |size| size.get() as usize),
                    )),
                    None,
                ));
                availability.push(routine.id.clone());
            }
            RoutineKind::ProgressiveOneShot | RoutineKind::ProgressiveTurducken => {
                let Some(model) = routine.model.as_deref().and_then(&mut model_for) else {
                    availability.push(format!("{}:generation-failed", routine.id));
                    failed_routines.push(routine.id.clone());
                    continue;
                };
                let parts = routine.parts.as_deref().unwrap_or_default();
                let mut previous = String::new();
                let mut ready = 0;
                match routine.kind {
                    RoutineKind::ProgressiveOneShot => {
                        let labels: Vec<_> = parts.iter().map(|part| part.id.as_str()).collect();
                        let instructions = format!(
                            "Summarize the source as JSON {{\"parts\":[{{\"id\":string,\"text\":string}}]}} with exactly these ordered additive part IDs: {}. Each text is new information not already in an earlier part.",
                            labels.join(", ")
                        );
                        if let Some(output) = complete(model.as_ref(), &source, "", &instructions)
                            && let Ok(parsed) = serde_json::from_str::<OneShotResponse>(&output)
                        {
                            for (part, supplied) in parts.iter().zip(&parsed.parts) {
                                if part.id != supplied.id || supplied.text.is_empty() {
                                    break;
                                }
                                let allowance =
                                    part.max_bytes.get().saturating_sub(previous.len() as u64)
                                        as usize;
                                let text = bounded(&supplied.text, allowance);
                                if text.is_empty() {
                                    break;
                                }
                                previous.push_str(&text);
                                representations.push(derived(
                                    &source_rep,
                                    routine,
                                    Some(text),
                                    Some(&part.id),
                                ));
                                ready += 1;
                            }
                        }
                    }
                    RoutineKind::ProgressiveTurducken => {
                        for part in parts {
                            let instructions = format!(
                                "Write only new additive summary information for part {}. Do not repeat earlier parts.",
                                part.id
                            );
                            let Some(output) =
                                complete(model.as_ref(), &source, &previous, &instructions)
                            else {
                                break;
                            };
                            let allowance =
                                part.max_bytes.get().saturating_sub(previous.len() as u64) as usize;
                            let text = bounded(&output, allowance);
                            if text.is_empty() {
                                break;
                            }
                            previous.push_str(&text);
                            representations.push(derived(
                                &source_rep,
                                routine,
                                Some(text),
                                Some(&part.id),
                            ));
                            ready += 1;
                        }
                    }
                    RoutineKind::Original | RoutineKind::Prefix => unreachable!(),
                }
                if ready < parts.len() {
                    availability.push(format!("{}:generation-failed", routine.id));
                    failed_routines.push(routine.id.clone());
                }
                for part in &parts[..ready] {
                    availability.push(format!("{}:part:{}", routine.id, part.id));
                }
            }
        }
    }
    Prepared {
        include_chat,
        representations,
        exchanges,
        original,
        availability,
        failed_routines,
    }
}

pub(crate) struct PostReply {
    pub key: ConversationKey,
    pub granted: Vec<String>,
    pub access: AssetAccess,
    pub publication: Option<crate::asset::Publication>,
    pub window: MemoryWindow,
    pub turn: ConversationTurn,
}

pub(crate) fn report_policy(policy: &ResolvedPolicy, recall: crate::config::RecallSource) {
    let mode = match policy.selection.mode {
        crate::config::memory::SelectionMode::Fixed => "fixed",
        crate::config::memory::SelectionMode::AgentPreference => "agentPreference",
        crate::config::memory::SelectionMode::Decider => "decider",
    };
    let bound = |value| match value {
        crate::config::memory::CaptureBound::Bytes(bytes) => bytes.get().to_string(),
        crate::config::memory::CaptureBound::Word(_) => "uncapped".to_owned(),
    };
    tracing::info!(
        event = "gateway_memory_policy",
        mode,
        recall = match recall {
            crate::config::RecallSource::None => "none",
            crate::config::RecallSource::Journal => "journal",
            crate::config::RecallSource::Platform => "platform",
        },
        retained_routines = policy.retain.len(),
        capture_user_bytes = %bound(policy.capture.user),
        capture_assistant_bytes = %bound(policy.capture.assistant),
        capture_tool_request_bytes = %bound(policy.capture.tool_request),
        capture_tool_result_bytes = %bound(policy.capture.tool_result),
        "gateway.memory.policy"
    );
}

pub(crate) async fn persist(
    runner: &SessionRunner,
    route: &BoundRoute,
    cancellation: &SessionCancellation,
    reply: PostReply,
) {
    persist_turn(runner, route, cancellation, reply)
        .instrument(tracing::info_span!("memory.persist"))
        .await;
}

async fn persist_turn(
    runner: &SessionRunner,
    route: &BoundRoute,
    cancellation: &SessionCancellation,
    reply: PostReply,
) {
    let PostReply {
        key,
        granted,
        access,
        publication,
        window,
        turn,
    } = reply;
    let Some(journal) = runner.journal.as_ref() else {
        return;
    };
    let journal = Arc::clone(journal);
    let policy = route.memory_policy.clone().unwrap_or_default();
    let models = Arc::clone(&runner.models);
    let anchor = Arc::clone(&route.model);
    let metering = Arc::clone(&runner.metering);
    let agent = route.agent.clone();
    let runtime = tokio::runtime::Handle::current();
    let cancel = cancellation.model_watch();
    let stem = key.journal_stem();
    let grant = journal::grant_digest(&granted);
    let inventory = runner.assets.inventory(&access);
    let next_asset_id = access.next_asset_id();
    let subscriber = tracing::dispatcher::get_default(Clone::clone);
    let span = tracing::Span::current();
    let include_chat = window.recall == crate::config::RecallSource::Journal;
    let generation_policy = policy.clone();
    let generation_turn = turn.clone();
    let generation_subscriber = subscriber.clone();
    let generation_span = span.clone();
    let generated = tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&generation_subscriber, || {
            let _entered = generation_span.enter();
            Some(prepare(
                &generation_turn,
                &generation_policy,
                |name| {
                    let config = generation_policy.models.get(name)?;
                    let client = models
                        .client_anchored(&anchor, config, runtime.clone(), cancel.clone())
                        .ok()?;
                    Some(crate::metering::ChatProviderGovernor::wrap(
                        client, &metering, &agent, config,
                    ))
                },
                include_chat,
            ))
        })
    })
    .await;
    let prepared = match generated {
        Ok(prepared) => prepared,
        Err(_) => {
            tracing::warn!(
                event = "gateway_memory_deviation",
                reason = "generation-task",
                "gateway.memory.deviation"
            );
            Some(prepare(&turn, &policy, |_| None, include_chat))
        }
    };
    if let Some(prepared) = &prepared {
        for failed in &prepared.failed_routines {
            tracing::warn!(
                event = "gateway_memory_deviation",
                reason = "generation-failed",
                routine = %failed.chars().take(64).collect::<String>(),
                "gateway.memory.deviation"
            );
        }
    }
    if !access.generation().is_some_and(|generation| {
        runner
            .conversations
            .publication_eligible(&key, &granted, generation)
    }) {
        tracing::warn!(
            event = "gateway_memory_retention_failed",
            reason = "stale-generation",
            "gateway.memory.retention.failed"
        );
        return;
    }
    let published = tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&subscriber, || {
            let _entered = span.enter();
            let _publication = publication;
            journal.append(
                &stem,
                &journal::Entry {
                    at: SystemTime::now(),
                    grant: &grant,
                    turn: &turn,
                    inventory: &inventory,
                    next_asset_id,
                    prepared: prepared.as_ref(),
                },
                window,
            )
        })
    })
    .await;
    let reason = match published {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error.label(),
        Err(_) => "task",
    };
    tracing::warn!(
        event = "gateway_journal_append_failed",
        reason,
        "gateway.journal.append.failed"
    );
}
