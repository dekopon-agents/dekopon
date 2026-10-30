use std::{
    num::{NonZeroU8, NonZeroU32},
    os::unix::fs::{FileExt as _, MetadataExt as _, PermissionsExt as _},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize},
    },
    time::Instant,
};

use dekopon_agent::{
    memory::{Kept, Reasoning},
    memory_tools::{MemoryRefusal, MemoryTools as _, Rung},
    prompt::HistoryLimits,
};
use dekopon_model::{
    model::{Backend, ModelKey, ModelMessage, OpaqueReasoning},
    text::Text,
};
use serde_json::{Value, json};

use super::*;
use crate::{
    asset::{AssetAccess, AssetFence, AssetStore},
    config::{MemoryScope, RecallSource},
    conversation::ConversationKey,
    memory::{
        project::{ProjectionInput, project},
        record::{ChoiceLine, PinLine, SlotLine, Subject, SummaryError, SummaryFailure},
    },
    tests::{captured, journal_lines, journal_records, message_text},
};
use tools::Admission;

const RETENTION: Duration = Duration::from_secs(3600);
const STEM: &str = "reviewer/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn grant() -> GrantDigest {
    GrantDigest::of(&["probe.read".to_owned()])
}

fn window() -> MemoryWindow {
    MemoryWindow {
        scope: MemoryScope::PrivateConversation,
        idle_timeout: Duration::from_secs(300),
        limits: HistoryLimits {
            max_turns: 8,
            max_bytes: 64 * 1024,
        },
        recall: RecallSource::Journal,
        forget_after: RETENTION,
    }
}

fn open(dir: &Path) -> Journal {
    Journal::open(dir, RETENTION, RETENTION).unwrap()
}

fn plan(parts: usize) -> SummaryPlan {
    SummaryPlan {
        model: "summary".to_owned(),
        parts: vec![NonZeroU32::new(8).unwrap(); parts],
        max_input_bytes: NonZeroU32::new(128 * 1024).unwrap(),
    }
}

fn append_turn(
    journal: &Journal,
    stem: &str,
    at: SystemTime,
    turn: &CapturedTurn,
    plan: Option<SummaryPlan>,
) -> RecordId {
    journal
        .append(
            stem,
            &Entry {
                at,
                grant: &grant(),
                captured: turn,
                keep: Keep::Everything,
                plan,
                attachments: Default::default(),
                arrivals: &[],
                inventory: &[],
                next_asset_id: 1,
            },
        )
        .unwrap()
        .expect("within the agent cap")
}

fn append_user(journal: &Journal, stem: &str, at: SystemTime, user: &str) -> RecordId {
    append_turn(journal, stem, at, &captured(user, Some("answer")), None)
}

fn slot(record: RecordId, part: u8, slot: Slot) -> SlotLine {
    SlotLine {
        at_ms: 1,
        record,
        part: NonZeroU8::new(part).unwrap(),
        slot,
        subject: Subject::Record,
    }
}

fn pin(record: RecordId, rung: Option<Rung>) -> PinLine {
    PinLine {
        at_ms: 1,
        record,
        pin: rung,
        subject: Subject::Record,
    }
}

fn tools_for<'a>(journal: &'a Journal, grant: GrantDigest) -> JournalMemoryTools<'a> {
    JournalMemoryTools::new(
        journal,
        STEM.to_owned(),
        Admission {
            grant,
            horizon_ms: 0,
            sealed_at_ms: None,
        },
    )
}

fn persist_pins(journal: &Journal, tools: JournalMemoryTools<'_>) {
    for line in tools.into_pins() {
        assert!(journal.write_pin(STEM, line).unwrap());
    }
}

fn recalled_pins(journal: &Journal) -> Vec<Option<Rung>> {
    journal
        .recall(STEM, &grant(), window(), SystemTime::now())
        .unwrap()
        .snapshot
        .unwrap()
        .records
        .iter()
        .map(|head| head.header.pin)
        .collect()
}

fn users(path: &Path) -> Vec<String> {
    journal_records(path)
        .iter()
        .map(|(_, body)| body[0]["user"]["text"].as_str().unwrap().to_owned())
        .collect()
}

fn side_records(path: &Path) -> Vec<u64> {
    journal_lines(path)
        .iter()
        .filter_map(|line| ["s", "p", "c"].iter().find_map(|tag| line.get(*tag)))
        .map(|side| side["record"].as_u64().unwrap())
        .collect()
}

fn header_ids(path: &Path) -> Vec<u64> {
    journal_records(path)
        .iter()
        .map(|(header, _)| header["id"].as_u64().unwrap())
        .collect()
}

#[test]
fn an_unlocked_reader_never_resets_a_journal() {
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let first = append_user(&journal, STEM, SystemTime::now(), "first");
    let done = AtomicBool::new(false);
    let reads = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            while !done.load(Ordering::Acquire) {
                let lookup = journal.record_lookup(STEM, first).unwrap().unwrap();
                assert_eq!(lookup.records[0].header.id, first);
                let recalled = journal
                    .recall(STEM, &grant(), window(), SystemTime::now())
                    .unwrap();
                assert_eq!(recalled.snapshot.unwrap().records[0].header.id, first);
                reads.fetch_add(1, Ordering::Relaxed);
            }
        });
        for _ in 0..4 {
            append_user(&journal, STEM, SystemTime::now(), &"x".repeat(256 * 1024));
        }
        done.store(true, Ordering::Release);
        reader.join().unwrap();
    });
    assert!(reads.load(Ordering::Relaxed) > 0);
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|head| head.header.id.0.get())
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
}

#[test]
fn an_unlocked_reader_keeps_its_snapshot_across_maintain() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(dir.path());
    let now = SystemTime::now();
    append_user(&journal, STEM, now - Duration::from_secs(1), "evicted turn");
    let kept = append_user(&journal, STEM, now, "kept turn");
    let path = journal.path(STEM);
    let inode = fs::metadata(&path).unwrap().ino();
    let snapshot = journal
        .recall(STEM, &grant(), window(), now)
        .unwrap()
        .snapshot
        .unwrap();
    let model = ModelKey::new(Backend::OpenAiCompatible, "model");
    let projected = project(&ProjectionInput {
        file: &snapshot.file,
        records: &snapshot.records,
        prior: &[],
        hot_turns: 8,
        max_turns: 8,
        max_bytes: 64 * 1024,
        model: &model,
    })
    .unwrap();
    let before: Vec<_> = projected.messages.iter().map(message_text).collect();
    assert!(before.iter().any(|(_, text)| text.contains("evicted turn")));
    std::thread::scope(|scope| {
        let guard = journal.hold_io_for_test();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let reader = &journal;
        let worker = scope.spawn(move || {
            let recalled = reader.recall(STEM, &grant(), window(), now).is_ok();
            let sealed = reader.sealed(STEM, RETENTION).is_ok();
            let read = tools_for(reader, grant()).read(kept.0.get(), "native", None);
            tx.send((recalled, sealed, read)).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(3));
        drop(guard);
        worker.join().unwrap();
        let (recalled, sealed, read) = result.expect("readers never wait on the writer lock");
        assert!(recalled && sealed);
        assert!(read.unwrap().contains("kept turn"));
    });
    let len = fs::metadata(&path).unwrap().len();
    journal.agent_bytes.insert("reviewer".to_owned(), len - 1);
    journal.maintain(STEM, now, 1);
    assert_ne!(fs::metadata(&path).unwrap().ino(), inode);
    let after: Vec<_> = projected.messages.iter().map(message_text).collect();
    assert_eq!(after, before);
    let fresh = journal
        .recall(STEM, &grant(), window(), now)
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(
        fresh
            .records
            .iter()
            .map(|head| head.header.id)
            .collect::<Vec<_>>(),
        [kept]
    );
}

#[test]
fn an_evicted_record_leaves_no_side_lines() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(dir.path());
    let now = SystemTime::now();
    let swept = append_turn(
        &journal,
        STEM,
        now - 2 * RETENTION,
        &captured("swept", None),
        Some(plan(2)),
    );
    let evicted = append_turn(
        &journal,
        STEM,
        now - Duration::from_secs(1),
        &captured("evicted", None),
        Some(plan(2)),
    );
    let kept = append_user(&journal, STEM, now, "kept");
    for record in [swept, evicted] {
        assert!(
            journal
                .write_slot(STEM, slot(record, 1, Slot::Generated("part".to_owned())))
                .unwrap()
        );
        assert!(
            journal
                .write_slot(
                    STEM,
                    SlotLine {
                        subject: Subject::Asset(1.try_into().unwrap()),
                        ..slot(record, 1, Slot::Running)
                    }
                )
                .unwrap()
        );
        assert!(
            journal
                .write_pin(STEM, pin(record, Some(Rung::Metadata)))
                .unwrap()
        );
        assert!(
            journal
                .write_choice(
                    STEM,
                    ChoiceLine {
                        at_ms: 1,
                        record,
                        choice: Rung::Native,
                    }
                )
                .unwrap()
        );
    }
    assert!(
        journal
            .write_pin(STEM, pin(kept, Some(Rung::Native)))
            .unwrap()
    );
    let path = journal.path(STEM);
    journal.maintain(STEM, now, 1);
    assert_eq!(header_ids(&path), [evicted.0.get(), kept.0.get()]);
    assert!(!side_records(&path).contains(&swept.0.get()));
    assert!(side_records(&path).contains(&evicted.0.get()));
    let len = fs::metadata(&path).unwrap().len();
    journal.agent_bytes.insert("reviewer".to_owned(), len - 1);
    journal.maintain(STEM, now, 1);
    assert_eq!(header_ids(&path), [kept.0.get()]);
    assert!(side_records(&path).is_empty());
    assert_eq!(recalled_pins(&journal), [Some(Rung::Native)]);
}

#[test]
fn no_reader_decodes_a_body() {
    records::DECODED_BYTES.set(0);
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(dir.path());
    let now = SystemTime::now();
    let large = captured(&"x".repeat(2 * 1024 * 1024), None);
    let mut ids = Vec::new();
    for at in [
        now - 2 * RETENTION,
        now - Duration::from_secs(2),
        now - Duration::from_secs(1),
        now,
    ] {
        let record = append_turn(&journal, STEM, at, &large, Some(plan(2)));
        assert!(
            journal
                .write_slot(
                    STEM,
                    slot(record, 1, Slot::Generated("part one".to_owned()))
                )
                .unwrap()
        );
        assert!(
            journal
                .write_choice(
                    STEM,
                    ChoiceLine {
                        at_ms: 1,
                        record,
                        choice: Rung::Summary(NonZeroU8::MIN),
                    }
                )
                .unwrap()
        );
        assert!(
            journal
                .write_pin(STEM, pin(record, Some(Rung::Metadata)))
                .unwrap()
        );
        ids.push(record);
    }
    journal.maintain(STEM, now, 1);
    let bodies = |journal: &Journal| -> Vec<(RecordId, Vec<u8>)> {
        let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
        snapshot
            .records
            .iter()
            .map(|head| {
                let mut body = vec![0; usize::try_from(head.header.body_len).unwrap()];
                snapshot
                    .file
                    .read_exact_at(&mut body, head.body_at)
                    .unwrap();
                (head.header.id, body)
            })
            .collect()
    };
    let swept = bodies(&journal);
    assert_eq!(swept.len(), 3);
    let len = fs::metadata(journal.path(STEM)).unwrap().len();
    journal.agent_bytes.insert("reviewer".to_owned(), len - 1);
    journal.maintain(STEM, now, 1);
    let capped = bodies(&journal);
    assert_eq!(capped.as_slice(), &swept[1..]);
    drop(journal);
    let journal = open(dir.path());
    let recalled = journal
        .recall(STEM, &grant(), window(), now)
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(
        recalled
            .records
            .iter()
            .map(|head| head.header.id)
            .collect::<Vec<_>>(),
        ids[2..]
    );
    for head in &recalled.records {
        assert_eq!(
            head.header.summaries[0],
            Slot::Generated("part one".to_owned())
        );
        assert_eq!(head.header.choice, Some(Rung::Summary(NonZeroU8::MIN)));
        assert_eq!(head.header.pin, Some(Rung::Metadata));
    }
    let tools = tools_for(&journal, grant());
    assert!(tools.read(ids[3].0.get(), "metadata", None).is_ok());
    tools.set_pin(ids[3].0.get(), "native").unwrap();
    persist_pins(&journal, tools);
    assert_eq!(bodies(&journal), capped);
    let decoded = records::DECODED_BYTES.get();
    assert!(decoded < 256 * 1024, "decoded {decoded} bytes");
}

#[test]
fn pin_ids_map_to_rungs() {
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let now = SystemTime::now();
    let other = append_user(&journal, STEM, now - Duration::from_secs(1), "other");
    let record = append_turn(
        &journal,
        STEM,
        now,
        &captured("the original", Some("its answer")),
        Some(plan(2)),
    );
    for (part, text) in [(1, "part one text"), (2, "part two text")] {
        assert!(
            journal
                .write_slot(STEM, slot(record, part, Slot::Generated(text.to_owned())))
                .unwrap()
        );
    }
    let id = record.0.get();
    let tools = tools_for(&journal, grant());
    assert_eq!(tools.read(id, "summary-2", None).unwrap(), "part two text");
    assert!(matches!(
        tools.read(id, "summary two", None),
        Err(MemoryRefusal::UnknownRepresentation { available })
            if available == [
                Rung::Native,
                Rung::Summary(NonZeroU8::new(1).unwrap()),
                Rung::Summary(NonZeroU8::new(2).unwrap()),
                Rung::Metadata,
            ]
    ));
    tools.set_pin(id, "summary-2").unwrap();
    tools.set_pin(other.0.get(), "native").unwrap();
    persist_pins(&journal, tools);
    assert_eq!(
        recalled_pins(&journal),
        [
            Some(Rung::Native),
            Some(Rung::Summary(NonZeroU8::new(2).unwrap()))
        ]
    );
    let tools = tools_for(&journal, grant());
    tools.set_pin(id, "metadata").unwrap();
    persist_pins(&journal, tools);
    assert_eq!(
        recalled_pins(&journal),
        [Some(Rung::Native), Some(Rung::Metadata)]
    );
    let tools = tools_for(&journal, grant());
    tools.clear_pin(None).unwrap();
    persist_pins(&journal, tools);
    journal.rewrite_records(STEM, |_| true).unwrap();
    assert_eq!(recalled_pins(&journal), [None, None]);
}

#[test]
fn a_summary_that_is_not_generated_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let now = SystemTime::now();
    let record = append_turn(
        &journal,
        STEM,
        now,
        &captured("original canary", None),
        Some(plan(1)),
    );
    let id = record.0.get();
    let tools = tools_for(&journal, grant());
    for state in [
        Slot::Error(SummaryError {
            kind: SummaryFailure::Refused,
            detail: "detail canary".to_owned(),
        }),
        Slot::Queued,
        Slot::Running,
    ] {
        assert!(journal.write_slot(STEM, slot(record, 1, state)).unwrap());
        for refusal in [
            tools.read(id, "summary-1", None).unwrap_err(),
            tools.set_pin(id, "summary-1").unwrap_err(),
        ] {
            let shown = refusal.to_string();
            assert!(!shown.contains("canary"), "{shown}");
            assert!(matches!(
                refusal,
                MemoryRefusal::RepresentationUnavailable { available }
                    if available == [Rung::Native, Rung::Metadata]
            ));
        }
    }
    assert!(tools.into_pins().is_empty());
    let tools = tools_for(&journal, grant());
    assert!(matches!(
        tools.read(id, "native", Some((4, 2))),
        Err(MemoryRefusal::Range { .. })
    ));
    assert!(matches!(
        tools.read(999, "native", None),
        Err(MemoryRefusal::RecordUnknown)
    ));
    let changed = tools_for(&journal, GrantDigest::of(&["probe.write".to_owned()]));
    assert!(matches!(
        changed.read(id, "native", None),
        Err(MemoryRefusal::RecordIneligible)
    ));
    assert!(matches!(
        changed.set_pin(id, "native"),
        Err(MemoryRefusal::RecordIneligible)
    ));
    journal
        .expire(now + 2 * RETENTION + Duration::from_secs(1))
        .unwrap();
    assert!(!journal.path(STEM).exists());
    assert!(matches!(
        tools.read(id, "native", None),
        Err(MemoryRefusal::RecordUnknown)
    ));
}

#[test]
fn reading_metadata_returns_the_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let record = append_user(&journal, STEM, SystemTime::now(), "user canary");
    let (header, _) = journal_records(&journal.path(STEM)).remove(0);
    let at = UNIX_EPOCH + Duration::from_millis(header["atMs"].as_u64().unwrap());
    let expected = format!(
        "[memory #{} · {} · 2 items · {} B]",
        record.0,
        humantime::format_rfc3339_seconds(at),
        header["bodyLen"]
    );
    let tools = tools_for(&journal, grant());
    assert_eq!(
        tools.read(record.0.get(), "metadata", None).unwrap(),
        expected
    );
    assert_eq!(
        tools
            .read(record.0.get(), "metadata", Some((0, 7)))
            .unwrap(),
        "[memory"
    );
}

#[test]
fn an_opaque_item_with_a_multiline_data_field_stays_on_one_line() {
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let model = ModelKey::new(Backend::Codex, "model");
    let data = "first line\nsecond line\r\n\tthird";
    let turn = CapturedTurn {
        original: vec![
            Item::User(Kept::new("question", usize::MAX)),
            Item::Reasoning(Reasoning::Opaque {
                model: model.clone(),
                bytes: OpaqueReasoning::from_value(
                    &json!({"type": "reasoning", "encrypted_content": data}),
                )
                .unwrap(),
            }),
            Item::Assistant(Kept::new("answer", usize::MAX)),
        ],
        model,
    };
    append_turn(&journal, STEM, SystemTime::now(), &turn, None);
    let lines = journal_lines(&journal.path(STEM));
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0]["b"][1]["reasoning"]["Opaque"]["bytes"]["encrypted_content"],
        data
    );
    assert!(lines[1].get("h").is_some());
}

#[test]
fn a_source_less_row_survives_two_append_reopen_cycles() {
    let key = ConversationKey::private(
        &"reviewer".parse().unwrap(),
        "whatsapp-test",
        "15550001111",
        &"tel.16034700182".parse().unwrap(),
    );
    let stem = key.journal_stem();
    let whatsapp = |index: usize| AssetSourceRef::WhatsApp {
        media_id: format!("media-{index}"),
        mime: "image/jpeg".to_owned(),
    };
    let window = MemoryWindow {
        forget_after: Duration::from_secs(30 * 24 * 60 * 60),
        ..window()
    };
    for source in [None, Some(whatsapp(0))] {
        let dir = tempfile::tempdir().unwrap();
        let at = UNIX_EPOCH + Duration::from_millis(millis(SystemTime::now()));
        let store = AssetStore::new(8, RETENTION);
        let access = AssetAccess::persistent(key.clone(), 1, Arc::new(AssetFence::new()));
        let pending = (0..2)
            .map(|index| PendingAsset {
                name: format!("photo-{index}.jpg"),
                mime: "image/jpeg".to_owned(),
                size: Some(1000),
                source: Some(whatsapp(index)),
            })
            .collect();
        let mut inventory = store
            .assets_for_access(&access, pending, Instant::now())
            .inventory;
        inventory[0].source = source;
        let arrived = if inventory[0].source.is_some() {
            at - WHATSAPP_MEDIA_LIFETIME - Duration::from_millis(1)
        } else {
            at
        };
        inventory[0].arrived = arrived;
        inventory[1].source = Some(AssetSourceRef::Generated {
            capability: "image.generate".into(),
            invocation: "generated".into(),
        });
        let turn = captured("attachments", Some("kept"));
        for _ in 0..2 {
            let journal = open(dir.path());
            journal
                .append(
                    &stem,
                    &Entry {
                        at,
                        grant: &grant(),
                        captured: &turn,
                        keep: Keep::Everything,
                        plan: None,
                        attachments: Default::default(),
                        arrivals: &[],
                        inventory: &inventory,
                        next_asset_id: 3,
                    },
                )
                .unwrap()
                .unwrap();
            drop(journal);
            let journal = open(dir.path());
            let recalled = journal.recall(&stem, &grant(), window, at).unwrap();
            assert_eq!(recalled.next_asset_id, 3);
            assert_eq!(recalled.assets.len(), 1);
            let row = &recalled.assets[0];
            assert_eq!(row.id, 1);
            assert_eq!(row.asset.name, "photo-0.jpg");
            assert_eq!(row.asset.mime, "image/jpeg");
            assert_eq!(row.asset.size, Some(1000));
            assert_eq!(row.arrived, arrived);
            assert!(row.asset.source.is_none());
            let store = AssetStore::new(8, RETENTION);
            let access = AssetAccess::persistent(key.clone(), 1, Arc::new(AssetFence::new()));
            store.restore(
                &access,
                recalled.assets,
                recalled.next_asset_id,
                Instant::now(),
            );
            assert_eq!(access.next_asset_id(), 3);
            inventory = store.inventory(&access);
        }
    }
}

#[test]
fn agent_directories_keep_independent_record_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let now = SystemTime::now();
    let first =
        ConversationKey::shared(&"reviewer".parse().unwrap(), "slack", "same").journal_stem();
    let other =
        ConversationKey::shared(&"another".parse().unwrap(), "slack", "same").journal_stem();
    append_user(&journal, &first, now, "first");
    append_user(&journal, &other, now, "other");
    append_user(&journal, &first, now, "second");
    assert_eq!(header_ids(&journal.path(&first)), [1, 2]);
    assert_eq!(header_ids(&journal.path(&other)), [1]);
    assert_ne!(journal.path(&first).parent(), journal.path(&other).parent());
}

#[test]
fn the_agent_cap_evicts_the_oldest_whole_record_across_conversations() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(dir.path());
    let now = SystemTime::now();
    let agent = "reviewer".parse().unwrap();
    let first = ConversationKey::shared(&agent, "slack", "first").journal_stem();
    let second = ConversationKey::shared(&agent, "slack", "second").journal_stem();
    let other =
        ConversationKey::shared(&"another".parse().unwrap(), "slack", "third").journal_stem();
    append_user(&journal, &first, now - Duration::from_secs(2), "middle");
    append_user(&journal, &second, now - Duration::from_secs(3), "old");
    append_user(&journal, &first, now - Duration::from_secs(1), "new");
    append_user(&journal, &other, now, "other agent");
    let size = |stem: &str| fs::metadata(journal.path(stem)).map_or(0, |meta| meta.len());
    let limit = size(&first);
    let other_before = fs::read(journal.path(&other)).unwrap();
    journal.agent_bytes.insert("reviewer".to_owned(), limit);
    journal.maintain(&first, now, 1);
    assert_eq!(users(&journal.path(&first)), ["middle", "new"]);
    assert!(!journal.path(&second).exists());
    assert_eq!(fs::read(journal.path(&other)).unwrap(), other_before);
    append_user(&journal, &first, now, "after cap");
    assert_eq!(header_ids(&journal.path(&first)), [1, 2, 3]);
}

#[test]
fn a_turn_larger_than_the_agent_cap_is_refused_without_evicting_older_turns() {
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(dir.path());
    let now = SystemTime::now();
    let agent = "reviewer".parse().unwrap();
    let stem = ConversationKey::shared(&agent, "slack", "first").journal_stem();
    let other = ConversationKey::shared(&agent, "slack", "other").journal_stem();
    append_user(&journal, &stem, now - Duration::from_secs(2), "kept");
    append_user(&journal, &other, now - Duration::from_secs(1), "also kept");
    let before = [
        fs::read(journal.path(&stem)).unwrap(),
        fs::read(journal.path(&other)).unwrap(),
    ];
    let cap = before.iter().map(|bytes| bytes.len() as u64).sum::<u64>() + 64;
    journal.agent_bytes.insert("reviewer".to_owned(), cap);
    let (capture, _guard) = crate::tests::capture_spans();
    let oversized = captured(&"x".repeat(usize::try_from(cap).unwrap()), None);
    assert!(
        journal
            .append(
                &stem,
                &Entry {
                    at: now,
                    grant: &grant(),
                    captured: &oversized,
                    keep: Keep::Everything,
                    plan: None,
                    attachments: Default::default(),
                    arrivals: &[],
                    inventory: &[],
                    next_asset_id: 1,
                },
            )
            .unwrap()
            .is_none()
    );
    journal.maintain(&stem, now, 1);
    let failures = capture.events_where(
        "dekopon_gatewayd",
        &[
            ("event", "gateway_memory_retention_failed"),
            ("reason", "agent-byte-cap"),
        ],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(
        [
            fs::read(journal.path(&stem)).unwrap(),
            fs::read(journal.path(&other)).unwrap()
        ],
        before
    );
}

#[test]
fn a_file_untouched_past_the_retention_is_deleted() {
    use tracing_subscriber::prelude::*;
    #[derive(Clone, Default)]
    struct RecordIds(Arc<parking_lot::Mutex<Vec<Vec<String>>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RecordIds {
        fn on_event(
            &self,
            _event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if let Some(array) = opentelemetry::Context::current()
                .get::<dekopon_telemetry::EventStringArray>()
                .filter(|array| array.name == "memory.record.ids")
            {
                self.0.lock().push(array.values.clone());
            }
        }
    }
    let capture = dekopon_test_support::CaptureLayer::workspace();
    let ids = RecordIds::default();
    let _guard = tracing_subscriber::registry()
        .with(capture.clone())
        .with(ids.clone())
        .set_default();
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let now = SystemTime::now();
    let agent = "reviewer".parse().unwrap();
    let [stale, fresh] = ["stale", "fresh"]
        .map(|conversation| ConversationKey::shared(&agent, "slack", conversation).journal_stem());
    let first = append_user(&journal, &stale, now, "stale");
    let second = append_user(&journal, &stale, now, "stale again");
    append_user(&journal, &fresh, now, "fresh");
    File::options()
        .write(true)
        .open(journal.path(&stale))
        .unwrap()
        .set_modified(now - RETENTION - Duration::from_secs(1))
        .unwrap();
    journal.maintain(&fresh, now, 1);
    assert!(!journal.path(&stale).exists());
    assert_eq!(users(&journal.path(&fresh)), ["fresh"]);
    let expired = capture.events_where(
        "dekopon_gatewayd",
        &[
            ("event", "gateway_journal_expired"),
            ("memory.expiry.reason", "file-age"),
        ],
    );
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].field("memory.records.evicted"), Some("2"));
    assert_eq!(
        *ids.0.lock(),
        [vec![first.0.to_string(), second.0.to_string()]]
    );
}

#[test]
fn a_v2_file_resets_at_boot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("{STEM}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let v2 = json!({
        "version": 2,
        "atMs": millis(SystemTime::now()),
        "grant": grant().hex(),
        "recordId": 1,
        "nextRecordId": 2,
        "sourceEligibility": true,
        "user": "old history",
        "answer": "old answer",
        "nextAssetId": 1,
    });
    fs::write(&path, format!("{v2}\n")).unwrap();
    let (journal, boot) = Journal::boot(
        dir.path(),
        RETENTION,
        RETENTION,
        RETENTION,
        NonZeroUsize::new(8).unwrap(),
        SystemTime::now(),
    )
    .unwrap();
    assert_eq!(boot.reset, 1);
    assert!(!path.exists());
    assert!(
        journal
            .recall(STEM, &grant(), window(), SystemTime::now())
            .unwrap()
            .snapshot
            .is_none()
    );
}

#[test]
fn a_seal_is_private_and_only_maintain_sweeps_a_corrupt_or_oversized_one() {
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let now = SystemTime::now();
    journal.seal(STEM, now).unwrap();
    let sealed = journal.seal_path(STEM);
    assert_eq!(
        fs::metadata(&sealed).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let value: Value = serde_json::from_slice(&fs::read(&sealed).unwrap()).unwrap();
    assert_eq!(value, json!({"atMs": millis(now)}));
    let corrupt = "reviewer/".to_owned() + &"c".repeat(64);
    let oversized = "reviewer/".to_owned() + &"d".repeat(64);
    fs::write(journal.seal_path(&corrupt), b"not a marker").unwrap();
    let mut marker = br#"{"atMs":1}"#.to_vec();
    marker.resize(usize::try_from(MAX_SEAL_BYTES).unwrap() + 1, b' ');
    fs::write(journal.seal_path(&oversized), marker).unwrap();
    for stem in [&corrupt, &oversized] {
        assert!(matches!(
            journal.sealed(stem, RETENTION),
            Err(JournalError::Corrupt)
        ));
        assert!(journal.seal_path(stem).exists());
    }
    journal.maintain(STEM, now, 1);
    assert!(!journal.seal_path(&corrupt).exists());
    assert!(!journal.seal_path(&oversized).exists());
    assert!(journal.sealed(STEM, RETENTION).unwrap().is_some());
}

#[test]
fn the_directory_and_its_files_are_private_to_the_daemon() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("journal");
    let journal = open(&dir);
    append_user(&journal, STEM, SystemTime::now(), "one");
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("reviewer")), 0o700);
    assert_eq!(mode(&journal.path(STEM)), 0o600);
}

#[test]
fn published_and_expired_records_name_their_ids_and_reason() {
    let (capture, _guard) = crate::tests::capture_spans();
    let dir = tempfile::tempdir().unwrap();
    let mut journal = open(dir.path());
    let now = SystemTime::now();
    let swept = append_turn(
        &journal,
        STEM,
        now - 2 * RETENTION,
        &captured("swept", Some("answer")),
        Some(plan(2)),
    );
    let capped = append_user(&journal, STEM, now - Duration::from_secs(1), "capped");
    let kept = append_user(&journal, STEM, now, "kept");
    journal.maintain(STEM, now, 1);
    let len = fs::metadata(journal.path(STEM)).unwrap().len();
    journal.agent_bytes.insert("reviewer".to_owned(), len - 1);
    journal.maintain(STEM, now, 1);
    assert_eq!(header_ids(&journal.path(STEM)), [kept.0.get()]);

    let records = capture.records();
    let published: Vec<_> = records
        .iter()
        .filter(|record| record.field("event") == Some("gateway_memory_published"))
        .collect();
    assert_eq!(published.len(), 3);
    assert_eq!(
        published[0].field("memory.record.id"),
        Some(swept.0.to_string().as_str())
    );
    assert_eq!(published[0].field("memory.record.items"), Some("2"));
    assert_eq!(published[0].field("memory.slots.queued"), Some("2"));
    let expired: Vec<_> = records
        .iter()
        .filter(|record| record.field("event") == Some("gateway_journal_expired"))
        .map(|record| {
            (
                record.field("memory.expiry.reason").unwrap().to_owned(),
                record.field("memory.records.evicted").unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        expired,
        [
            ("record-age".to_owned(), "1".to_owned()),
            ("agent-cap".to_owned(), "1".to_owned())
        ]
    );
    let _ = capped;
}

#[test]
fn a_warm_window_repacks_down_to_max_turns_on_count_alone() {
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let now = SystemTime::now();
    let mut limits = window();
    limits.limits.max_turns = 4;
    limits.limits.max_bytes = 1024 * 1024;
    let model = ModelKey::new(Backend::OpenAiCompatible, "model");
    let mut prior = Vec::new();
    let mut shown_after_repack = Vec::new();
    for turn in 0..12 {
        append_user(&journal, STEM, now, &format!("turn {turn}"));
        let snapshot = journal
            .recall(STEM, &grant(), limits, now)
            .unwrap()
            .snapshot
            .unwrap();
        let projected = project(&ProjectionInput {
            file: &snapshot.file,
            records: &snapshot.records,
            prior: &prior,
            hot_turns: 4,
            max_turns: 4,
            max_bytes: limits.limits.max_bytes,
            model: &model,
        })
        .unwrap();
        if projected.repacked {
            shown_after_repack.push(
                projected
                    .prior
                    .iter()
                    .filter(|prior| prior.rung.is_some())
                    .count(),
            );
        }
        prior = projected.prior;
    }
    assert!(!shown_after_repack.is_empty());
    assert!(shown_after_repack.iter().all(|shown| *shown == 4));
}

fn project_native(journal: &Journal) -> Vec<ModelMessage> {
    let snapshot = journal
        .recall(STEM, &grant(), window(), SystemTime::now())
        .unwrap()
        .snapshot
        .unwrap();
    project(&ProjectionInput {
        file: &snapshot.file,
        records: &snapshot.records,
        prior: &[],
        hot_turns: 8,
        max_turns: 8,
        max_bytes: 4 * 1024 * 1024,
        model: &ModelKey::new(Backend::OpenAiCompatible, "model"),
    })
    .unwrap()
    .messages
}

#[test]
fn an_argument_past_the_scratch_buffer_is_validated_without_leaving_the_journal() {
    use dekopon_agent::memory::{Call, Exchange, Outcome, Tool, ToolResult};
    use dekopon_model::replay::ReplayMessage;
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let large = "é\\\"".repeat(48 * 1024);
    let exchange = |id: &str, arguments: String| {
        Item::Exchange(Exchange {
            call: Call {
                id: id.into(),
                tool: Tool::Bash,
                arguments: Kept::new(&arguments, usize::MAX),
            },
            result: ToolResult {
                outcome: Outcome::Exit(0),
                text: Kept::new("done", usize::MAX),
            },
        })
    };
    let mut turn = captured("question", Some("answer"));
    turn.original
        .insert(1, exchange("object", json!({"command": large}).to_string()));
    turn.original
        .insert(2, exchange("array", json!([large]).to_string()));
    append_turn(&journal, STEM, SystemTime::now(), &turn, None);
    let arguments: Vec<_> = project_native(&journal)
        .into_iter()
        .filter_map(|message| match message {
            ModelMessage::Journal {
                message: ReplayMessage::Assistant { calls, .. },
            } => Some(calls),
            ModelMessage::Journal {
                message: ReplayMessage::User(_) | ReplayMessage::Tool { .. },
            }
            | ModelMessage::System { .. }
            | ModelMessage::User { .. }
            | ModelMessage::Assistant { .. }
            | ModelMessage::ToolResults { .. } => None,
        })
        .flatten()
        .map(|call| (call.id.as_str().to_owned(), call.arguments))
        .collect();
    assert_eq!(arguments.len(), 2);
    assert!(
        matches!(&arguments[0], (id, Text::File { len, .. }) if id == "object" && len > &(64 * 1024))
    );
    assert!(
        matches!(&arguments[1], (id, Text::Owned(cut)) if id == "array" && cut.starts_with("{\"cut\":"))
    );
}

#[test]
fn asset_markers_replay_as_nothing_beside_native_user_and_steer_text() {
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let mut turn = captured("question", None);
    turn.original = vec![
        Item::User(Kept::new("question", usize::MAX)),
        Item::Asset(1.try_into().unwrap()),
        Item::Steer(Kept::new("steer text", usize::MAX)),
        Item::Asset(2.try_into().unwrap()),
    ];
    append_turn(&journal, STEM, SystemTime::now(), &turn, None);
    drop(journal);
    let journal = open(dir.path());
    let messages = project_native(&journal);
    let texts: Vec<_> = messages.iter().map(message_text).collect();
    assert_eq!(
        texts
            .iter()
            .filter(|(role, _)| role == "user")
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        ["question", "steer text"]
    );
    for message in &messages {
        let encoded = serde_json::to_string(&message_text(message)).unwrap();
        assert!(!encoded.contains("asset"), "{encoded}");
    }
}

#[test]
fn a_lone_surrogate_argument_replays_as_a_cut_after_reopen() {
    use dekopon_agent::memory::{Call, Exchange, Outcome, Tool, ToolResult};
    use dekopon_model::replay::ReplayMessage;
    let dir = tempfile::tempdir().unwrap();
    let journal = open(dir.path());
    let mut turn = captured("question", Some("answer"));
    for (index, arguments) in [r#"{"\ud800":0}"#, r#"{"😀":0}"#].into_iter().enumerate() {
        turn.original.insert(
            1 + index,
            Item::Exchange(Exchange {
                call: Call {
                    id: format!("call-{index}").as_str().into(),
                    tool: Tool::Bash,
                    arguments: Kept::new(arguments, usize::MAX),
                },
                result: ToolResult {
                    outcome: Outcome::Exit(0),
                    text: Kept::new("done", usize::MAX),
                },
            }),
        );
    }
    append_turn(&journal, STEM, SystemTime::now(), &turn, None);
    drop(journal);
    let journal = open(dir.path());
    let arguments: Vec<_> = project_native(&journal)
        .into_iter()
        .filter_map(|message| match message {
            ModelMessage::Journal {
                message: ReplayMessage::Assistant { calls, .. },
            } => Some(calls),
            ModelMessage::Journal {
                message: ReplayMessage::User(_) | ReplayMessage::Tool { .. },
            }
            | ModelMessage::System { .. }
            | ModelMessage::User { .. }
            | ModelMessage::Assistant { .. }
            | ModelMessage::ToolResults { .. } => None,
        })
        .flatten()
        .map(|call| call.arguments)
        .collect();
    assert_eq!(arguments.len(), 2);
    assert!(matches!(&arguments[0], Text::Owned(cut) if cut == r#"{"cut":12}"#));
    assert!(matches!(&arguments[1], Text::File { .. }));
}
