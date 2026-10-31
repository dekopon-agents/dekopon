use std::{num::NonZeroU8, sync::Arc, time::SystemTime};

use dekopon_agent::memory::CapturedTurn;
use tracing::Instrument as _;

use super::{
    engine::SlotTask,
    journal::{self, CorruptStem, Keep},
    record::{ChoiceLine, GrantDigest, PinLine, RecordId, SummaryPlan},
};
use crate::{
    asset::AssetAccess,
    config::{RecallSource, memory::SummaryModel},
    conversation::ConversationKey,
    follow::PendingFollow,
    routes::BoundRoute,
    session::SessionRunner,
};

pub(crate) struct PostReply {
    pub key: ConversationKey,
    pub granted: Vec<String>,
    pub access: AssetAccess,
    pub publication: Option<crate::asset::Publication>,
    pub captured: CapturedTurn,
    pub arrivals: Vec<Vec<u64>>,
    pub attachments: crate::config::memory::AttachmentSummaries,
    pub keep: Keep,
    pub plan: Option<SummaryPlan>,
    pub recalled: Option<journal::records::Snapshot>,
}

/// A turn's journal writes; nothing here touches disk until the session drains it after the
/// reply's terminal is delivered.
#[derive(Default)]
pub(crate) struct Deferred {
    pub corrupt: Option<CorruptStem>,
    pub follows: Vec<PendingFollow>,
    pub pins: Vec<PinLine>,
    pub choice: Option<ChoiceLine>,
    pub record: Option<PostReply>,
}

pub(crate) fn plan(route: &BoundRoute) -> Option<SummaryPlan> {
    let summaries = route.memory_knobs.as_ref()?.summaries.as_ref()?;
    Some(SummaryPlan {
        model: match &summaries.model {
            SummaryModel::Route => route.model.name().to_owned(),
            SummaryModel::Named(name) => name.clone(),
        },
        parts: summaries.parts.clone(),
        max_input_bytes: summaries.max_input_bytes,
    })
}

pub(crate) const fn keep(recall: RecallSource) -> Keep {
    match recall {
        RecallSource::Journal => Keep::Everything,
        RecallSource::None | RecallSource::Platform => Keep::ExchangesOnly,
    }
}

pub(crate) fn report_policy(route: &BoundRoute) {
    let (Some(window), Some(knobs)) = (route.memory.window(), route.memory_knobs.as_ref()) else {
        return;
    };
    let summaries = plan(route);
    tracing::info!(
        event = "gateway_memory_policy",
        recall = match window.recall {
            RecallSource::None => "none",
            RecallSource::Journal => "journal",
            RecallSource::Platform => "platform",
        },
        memory.hot_turns = knobs.hot_turns.get(),
        memory.summaries.parts = knobs.summaries.as_ref().map_or(0, |plan| plan.parts.len()),
        memory.summaries.model = summaries.as_ref().map(|plan| plan.model.as_str()),
        memory.summaries.max_input_bytes =
            summaries.as_ref().map(|plan| plan.max_input_bytes.get()),
        memory.capture.max_item_bytes = knobs.capture_max_item_bytes.get(),
        memory.selection = match knobs.selection {
            crate::config::memory::Selection::Ladder => "ladder",
            crate::config::memory::Selection::Decider(_) => "decider",
        },
        "gateway.memory.policy"
    );
}

fn pin_dropped(record: RecordId, subject: super::record::Subject, reason: &'static str) {
    tracing::info!(
        event = "gateway_memory_pin_dropped",
        memory.record.id = record.0.get(),
        memory.pin.reason = reason,
        memory.summary.subject = subject.label(),
        "gateway.memory.pin.dropped"
    );
}

pub(crate) async fn drain(runner: &SessionRunner, deferred: Deferred) {
    drain_turn(runner, deferred)
        .instrument(tracing::info_span!("memory.persist"))
        .await;
    runner.assets.finish_publications().await;
}

struct Persisted {
    record: Option<SlotTask>,
    stem: String,
    access: AssetAccess,
    recalled: Vec<journal::records::Head>,
    appended: Option<journal::records::Header>,
}

async fn drain_turn(runner: &SessionRunner, deferred: Deferred) {
    let Deferred {
        corrupt,
        follows,
        pins,
        choice,
        record,
    } = deferred;
    if record.is_none() {
        for pin in &pins {
            pin_dropped(pin.record, pin.subject, "turn-not-persisted");
        }
    }
    let journal = runner.journal.as_ref().map(Arc::clone);
    let subscriber = tracing::dispatcher::get_default(Clone::clone);
    let span = tracing::Span::current();
    let store = Arc::clone(&runner.assets);
    let written = tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&subscriber, || {
            let _entered = span.enter();
            let journal = journal?;
            if let Some(corrupt) = corrupt {
                journal.reset(corrupt);
            }
            let reply = record?;
            let PostReply {
                key,
                granted,
                access,
                publication,
                captured,
                arrivals,
                attachments,
                keep,
                plan,
                recalled,
            } = reply;
            let stem = key.journal_stem();
            let next_asset_id = access.next_asset_id();
            let inventory = store.inventory(&access);
            let grant = GrantDigest::of(&granted);
            let planned = plan.is_some();
            let appended = journal.append(
                &stem,
                &journal::Entry {
                    at: SystemTime::now(),
                    grant: &grant,
                    captured: &captured,
                    arrivals: &arrivals,
                    attachments,
                    keep,
                    plan,
                    inventory: &inventory,
                    next_asset_id,
                },
            );
            if let Some(publication) = publication {
                publication.finish_blocking();
            }
            let appended = match appended {
                Ok(appended) => appended,
                Err(error) => {
                    tracing::warn!(
                        event = "gateway_journal_append_failed",
                        reason = error.label(),
                        "gateway.journal.append.failed"
                    );
                    None
                }
            };
            for pin in pins {
                let record = pin.record;
                let subject = pin.subject;
                if appended.is_none() || !matches!(journal.write_pin(&stem, pin), Ok(true)) {
                    pin_dropped(record, subject, "record-gone");
                }
            }
            if let Some(choice) = choice
                && appended.is_some()
            {
                let _written = journal.write_choice(&stem, choice);
            }
            journal.maintain(&stem, SystemTime::now(), next_asset_id);
            let record = appended
                .as_ref()
                .filter(|_| planned)
                .map(|header| SlotTask {
                    stem: stem.clone(),
                    record: header.id,
                    part: NonZeroU8::MIN,
                    subject: super::record::Subject::Record,
                    access: None,
                });
            Some(Persisted {
                record,
                stem,
                access,
                recalled: recalled.map_or_else(Vec::new, |snapshot| snapshot.records),
                appended,
            })
        })
    })
    .await;
    match written {
        Ok(Some(written)) => {
            if let Some(summaries) = runner.summaries.as_ref() {
                if let Some(task) = written.record {
                    summaries.enqueue(task).await;
                }
                summaries.sweep(
                    &written.stem,
                    &written.access,
                    written
                        .recalled
                        .iter()
                        .map(|head| (&head.header, head.asset_lines.as_slice()))
                        .chain(written.appended.iter().map(|header| (header, &[][..]))),
                );
            }
        }
        Ok(None) => {}
        Err(_) => tracing::warn!(
            event = "gateway_journal_append_failed",
            reason = "task",
            "gateway.journal.append.failed"
        ),
    }
    if !follows.is_empty() {
        let written = tokio::task::spawn_blocking(move || {
            for follow in follows {
                follow.write();
            }
        })
        .await;
        if written.is_err() {
            tracing::warn!(
                audit.event = "gateway.follow",
                operation = "refused",
                reason = "task",
                "gateway.follow"
            );
        }
    }
}
