use super::*;
use dekopon_agent::memory::{Call as CapturedCall, Exchange, ToolResult};
use dekopon_model::model::{Backend, OpaqueReasoning};
use std::{
    num::{NonZeroU8, NonZeroU32},
    time::Duration,
};

pub(crate) fn fixture() -> TurnRecord {
    TurnRecord {
        id: RecordId(NonZeroU64::new(1).unwrap()),
        turn: TurnId(1),
        at_ms: 1,
        grant: GrantDigest([0; 8]),
        model: ModelKey::new(Backend::Codex, "fixture"),
        original: vec![Item::User(Kept::new("a\n🦀\\\"z", 1024))],
        summaries: vec![Slot::Queued; 3],
        plan: Some(SummaryPlan {
            model: "summary".to_owned(),
            parts: vec![NonZeroU32::new(8).unwrap(); 3],
            max_input_bytes: NonZeroU32::new(128 * 1024).unwrap(),
        }),
        assets: Vec::new(),
        next_asset_id: 1,
    }
}

pub(crate) fn journal() -> (tempfile::TempDir, Arc<Journal>) {
    let dir = tempfile::tempdir().unwrap();
    let (journal, _boot) = Journal::open_records(
        dir.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        64.try_into().unwrap(),
    )
    .unwrap();
    let journal = Arc::new(journal);
    (dir, journal)
}

#[test]
fn body_spans_preserve_the_capture_encoding_and_item_numbers() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    record.original.extend([
        Item::Asset(1.try_into().unwrap()),
        Item::Steer(Kept::new("later", 1024)),
        Item::Asset(2.try_into().unwrap()),
        Item::Reasoning(Reasoning::Plain(Kept::new("plain", 1024))),
        Item::Reasoning(Reasoning::Opaque {
            model: record.model.clone(),
            bytes: OpaqueReasoning::from_value(
                &serde_json::json!({"type":"reasoning","value":"secret\nvalue"}),
            )
            .unwrap(),
        }),
        Item::Exchange(Exchange {
            call: CapturedCall {
                id: "call".into(),
                tool: Tool::Rejected(Kept::new("unknown", 1024)),
                arguments: Kept::new("{\"q\":1}", 1024),
            },
            result: ToolResult {
                outcome: Outcome::Failed,
                text: Kept::new("failure", 1024),
            },
        }),
    ]);
    journal
        .append_record("reviewer/record", &record, Vec::new())
        .unwrap();
    let snapshot = journal
        .record_snapshot("reviewer/record", true)
        .unwrap()
        .unwrap();
    let head = &snapshot.records[0];
    let mut body = vec![0; head.header.body_len as usize];
    snapshot
        .file
        .read_exact_at(&mut body, head.body_at)
        .unwrap();
    let expected = format!(
        "{{\"b\":{}}}\n",
        serde_json::to_string(&record.original).unwrap()
    );
    assert_eq!(body, expected.as_bytes());
    assert!(
        head.header
            .spans
            .iter()
            .all(|span| ![1, 3].contains(&span.item))
    );
    assert_eq!(head.header.spans[1].item, 2);
    for span in head
        .header
        .spans
        .iter()
        .filter(|span| matches!(span.kind, Kind::Escaped))
    {
        assert!(
            escaped_text(
                &snapshot.file,
                head.body_at + span.rel_offset,
                span.len,
                1024
            )
            .is_ok()
        );
    }
    let span = &head.header.spans[0];
    for cap in 0..15 {
        let value = escaped_text(
            &snapshot.file,
            head.body_at + span.rel_offset,
            span.len,
            cap,
        )
        .unwrap();
        assert!(
            record.original[0]
                .clone()
                .eq(&Item::User(Kept::new("a\n🦀\\\"z", 1024)))
        );
        assert!("a\n🦀\\\"z".starts_with(&value));
        assert!(value.len() <= cap);
    }
}

#[test]
fn an_r28_slot_line_parses_as_a_record_subject() {
    let line = SlotLine {
        at_ms: 2,
        record: fixture().id,
        part: NonZeroU8::new(1).unwrap(),
        slot: Slot::Running,
        subject: Subject::Record,
    };
    let raw = serde_json::to_string(&line).unwrap();
    assert!(!raw.contains("subject"));
    assert!(
        serde_json::from_str::<SlotLine>(&raw)
            .unwrap()
            .subject
            .is_record()
    );
}

#[test]
fn an_asset_slot_does_not_change_record_slots_in_the_walk() {
    let (_dir, journal) = journal();
    let record = fixture();
    journal
        .append_record("reviewer/record", &record, Vec::new())
        .unwrap();
    journal
        .write_slot(
            "reviewer/record",
            SlotLine {
                at_ms: 2,
                record: record.id,
                part: NonZeroU8::new(1).unwrap(),
                slot: Slot::Generated("asset only".to_owned()),
                subject: Subject::Asset(1.try_into().unwrap()),
            },
        )
        .unwrap();
    let snapshot = journal
        .record_snapshot("reviewer/record", true)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.records[0].header.summaries, vec![Slot::Queued; 3]);
    assert_eq!(snapshot.records[0].asset_lines.len(), 1);
}

#[test]
fn unknown_header_and_slot_fields_are_corrupt() {
    let (_dir, journal) = journal();
    let record = fixture();
    journal
        .append_record("reviewer/record", &record, Vec::new())
        .unwrap();
    let path = journal.path("reviewer/record");
    let raw = std::fs::read(&path).unwrap();
    let mut changed = String::from_utf8(raw.clone()).unwrap();
    changed = changed.replace("\"bodyLen\":", "\"unknown\":true,\"bodyLen\":");
    std::fs::write(&path, changed).unwrap();
    assert!(matches!(
        journal.record_snapshot("reviewer/record", true),
        Err(JournalError::Corrupt)
    ));
    std::fs::write(&path, raw).unwrap();
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "{{\"s\":{{\"atMs\":2,\"record\":1,\"part\":1,\"slot\":\"Running\",\"unknown\":true}}}}"
    )
    .unwrap();
    assert!(matches!(
        journal.record_snapshot("reviewer/record", true),
        Err(JournalError::Corrupt)
    ));
}

#[test]
fn a_torn_tail_is_skipped_by_an_unlocked_reader() {
    let (_dir, journal) = journal();
    journal
        .append_record("reviewer/record", &fixture(), Vec::new())
        .unwrap();
    let path = journal.path("reviewer/record");
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"b\":[\"unfinished\"]}\n{\"h\":{")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        journal
            .record_snapshot("reviewer/record", false)
            .unwrap()
            .unwrap()
            .records
            .len(),
        1
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert!(matches!(
        journal.record_snapshot("reviewer/record", true),
        Err(JournalError::Corrupt)
    ));
}

const STEM: &str = "reviewer/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn arrival(item: u32, number: u64) -> ArrivedAsset {
    ArrivedAsset {
        item,
        number: number.try_into().unwrap(),
        kind: dekopon_core::asset::AssetKind::Image,
        bytes: Some(12),
        summaries: Vec::new(),
    }
}

fn reopen(dir: &std::path::Path) -> (Journal, Boot) {
    Journal::open_records(
        dir,
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        64.try_into().unwrap(),
    )
    .unwrap()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct R28Slot {
    at_ms: u64,
    record: RecordId,
    part: NonZeroU8,
    slot: Slot,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct R28Pin {
    at_ms: u64,
    record: RecordId,
    pin: Option<dekopon_agent::memory_tools::Rung>,
}

#[test]
fn a_reader_without_subject_refuses_an_asset_line() {
    let line = SlotLine {
        at_ms: 2,
        record: fixture().id,
        part: NonZeroU8::new(1).unwrap(),
        slot: Slot::Running,
        subject: Subject::Asset(1.try_into().unwrap()),
    };
    assert!(serde_json::from_slice::<R28Slot>(&serde_json::to_vec(&line).unwrap()).is_err());
    let old = R28Slot {
        at_ms: 2,
        record: fixture().id,
        part: NonZeroU8::new(1).unwrap(),
        slot: Slot::Running,
    };
    assert!(
        serde_json::from_slice::<SlotLine>(&serde_json::to_vec(&old).unwrap())
            .unwrap()
            .subject
            .is_record()
    );
}

#[test]
fn a_record_without_arrivals_has_the_r28_encoding() {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct R28Header<'a> {
        id: RecordId,
        next_record_id: NonZeroU64,
        turn: TurnId,
        at_ms: u64,
        grant: &'a GrantDigest,
        model: &'a ModelKey,
        summaries: &'a [Slot],
        plan: &'a Option<SummaryPlan>,
        assets: &'a [AssetRow],
        next_asset_id: u64,
        body_len: u64,
        spans: &'a [Span],
        #[serde(skip_serializing_if = "Option::is_none")]
        pin: Option<dekopon_agent::memory_tools::Rung>,
        #[serde(skip_serializing_if = "Option::is_none")]
        choice: Option<dekopon_agent::memory_tools::Rung>,
    }
    let (_dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    let h = &snapshot.records[0].header;
    let old = R28Header {
        id: h.id,
        next_record_id: h.next_record_id,
        turn: h.turn,
        at_ms: h.at_ms,
        grant: &h.grant,
        model: &h.model,
        summaries: &h.summaries,
        plan: &h.plan,
        assets: &h.assets,
        next_asset_id: h.next_asset_id,
        body_len: h.body_len,
        spans: &h.spans,
        pin: None,
        choice: None,
    };
    assert_eq!(
        serde_json::to_vec(h).unwrap(),
        serde_json::to_vec(&old).unwrap()
    );
    let slot = SlotLine {
        at_ms: 2,
        record: h.id,
        part: NonZeroU8::new(1).unwrap(),
        slot: Slot::Running,
        subject: Subject::Record,
    };
    assert_eq!(
        serde_json::to_vec(&slot).unwrap(),
        serde_json::to_vec(&R28Slot {
            at_ms: 2,
            record: h.id,
            part: slot.part,
            slot: Slot::Running
        })
        .unwrap()
    );
    let pin = PinLine {
        at_ms: 3,
        record: h.id,
        pin: Some(dekopon_agent::memory_tools::Rung::Metadata),
        subject: Subject::Record,
    };
    assert_eq!(
        serde_json::to_vec(&pin).unwrap(),
        serde_json::to_vec(&R28Pin {
            at_ms: 3,
            record: h.id,
            pin: pin.pin
        })
        .unwrap()
    );
    let absent: PinLine = serde_json::from_slice(
        &serde_json::to_vec(&R28Pin {
            at_ms: 3,
            record: h.id,
            pin: None,
        })
        .unwrap(),
    )
    .unwrap();
    assert!(absent.subject.is_record());
}

#[test]
fn a_walk_returns_asset_lines_unapplied_in_file_order() {
    use dekopon_agent::memory_tools::Rung;
    let (_dir, journal) = journal();
    journal
        .append_record(STEM, &fixture(), vec![arrival(1, 1)])
        .unwrap();
    journal
        .write_pin(
            STEM,
            PinLine {
                at_ms: 2,
                record: fixture().id,
                pin: Some(Rung::Native),
                subject: Subject::Record,
            },
        )
        .unwrap();
    journal
        .write_slot(
            STEM,
            SlotLine {
                at_ms: 3,
                record: fixture().id,
                part: NonZeroU8::new(1).unwrap(),
                slot: Slot::Generated("asset first".to_owned()),
                subject: Subject::Asset(1.try_into().unwrap()),
            },
        )
        .unwrap();
    journal
        .write_pin(
            STEM,
            PinLine {
                at_ms: 4,
                record: fixture().id,
                pin: Some(Rung::Metadata),
                subject: Subject::Asset(1.try_into().unwrap()),
            },
        )
        .unwrap();
    journal
        .write_slot(
            STEM,
            SlotLine {
                at_ms: 5,
                record: fixture().id,
                part: NonZeroU8::new(2).unwrap(),
                slot: Slot::Generated("asset second".to_owned()),
                subject: Subject::Asset(1.try_into().unwrap()),
            },
        )
        .unwrap();
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    let head = &snapshot.records[0];
    assert_eq!(head.header.summaries, vec![Slot::Queued; 3]);
    assert_eq!(head.header.pin, Some(Rung::Native));
    let [
        AssetSideLine::Slot(first),
        AssetSideLine::Pin(pin),
        AssetSideLine::Slot(second),
    ] = head.asset_lines.as_slice()
    else {
        panic!("asset line order");
    };
    assert_eq!(first.slot, Slot::Generated("asset first".to_owned()));
    assert_eq!(pin.pin, Some(Rung::Metadata));
    assert_eq!(second.slot, Slot::Generated("asset second".to_owned()));
    journal.rewrite_records(STEM, |_| true).unwrap();
    let folded = journal.record_snapshot(STEM, true).unwrap().unwrap();
    assert_eq!(folded.records[0].header.summaries, vec![Slot::Queued; 3]);
    assert_eq!(folded.records[0].header.pin, Some(Rung::Native));
    assert_eq!(folded.records[0].asset_lines.len(), 3);
}

#[test]
fn rewrites_preserve_arrival_entries_and_asset_lines_byte_for_byte() {
    let (_dir, journal) = journal();
    journal
        .append_record(STEM, &fixture(), vec![arrival(1, 1)])
        .unwrap();
    journal
        .write_slot(
            STEM,
            SlotLine {
                at_ms: 3,
                record: fixture().id,
                part: NonZeroU8::new(1).unwrap(),
                slot: Slot::Running,
                subject: Subject::Asset(1.try_into().unwrap()),
            },
        )
        .unwrap();
    journal
        .write_pin(
            STEM,
            PinLine {
                at_ms: 4,
                record: fixture().id,
                pin: None,
                subject: Subject::Asset(1.try_into().unwrap()),
            },
        )
        .unwrap();
    let path = journal.path(STEM);
    let raw = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\"arrived\":[{", "\"arrived\":[  {")
        .replace("\"kind\":\"image\"", "\"kind\" : \"image\"")
        .replace("\"atMs\":3", "\"atMs\" : 3");
    std::fs::write(&path, raw.as_bytes()).unwrap();
    let before = journal.record_snapshot(STEM, true).unwrap().unwrap();
    let expected_arrived = before.records[0]
        .arrived_raw
        .as_ref()
        .unwrap()
        .get()
        .to_owned();
    let expected_lines = before.records[0].raw_asset_lines.clone();
    let expected_body = read_body(&before, 0);
    for _ in 0..2 {
        journal.rewrite_records(STEM, |_| true).unwrap();
        let after = journal.record_snapshot(STEM, true).unwrap().unwrap();
        assert_eq!(
            after.records[0].arrived_raw.as_ref().unwrap().get(),
            expected_arrived
        );
        assert_eq!(after.records[0].raw_asset_lines, expected_lines);
        assert_eq!(read_body(&after, 0), expected_body);
        assert!(after.records[0].header.arrived[0].summaries.is_empty());
    }
    journal.rewrite_records(STEM, |_| false).unwrap();
    let after = journal.record_snapshot(STEM, true).unwrap().unwrap();
    assert!(after.records.is_empty());
    assert_eq!(after.file.metadata().unwrap().len(), 0);
}

fn read_body(snapshot: &Snapshot, position: usize) -> Vec<u8> {
    let head = &snapshot.records[position];
    let mut body = vec![0; head.header.body_len as usize];
    snapshot
        .file
        .read_exact_at(&mut body, head.body_at)
        .unwrap();
    body
}

#[test]
fn an_arrival_with_empty_summaries_round_trips() {
    let asset = arrival(1, 7);
    let decoded: ArrivedAsset =
        serde_json::from_slice(&serde_json::to_vec(&asset).unwrap()).unwrap();
    assert_eq!(decoded.item, 1);
    assert_eq!(u64::from(decoded.number), 7);
    assert!(decoded.summaries.is_empty());
    let (_dir, journal) = journal();
    journal
        .append_record(STEM, &fixture(), vec![decoded])
        .unwrap();
    assert!(
        journal
            .record_snapshot(STEM, true)
            .unwrap()
            .unwrap()
            .records[0]
            .header
            .arrived[0]
            .summaries
            .is_empty()
    );
}

#[test]
fn an_unknown_pin_field_is_corrupt() {
    let (_dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    let mut file = OpenOptions::new()
        .append(true)
        .open(journal.path(STEM))
        .unwrap();
    file.write_all(b"{\"p\":{\"atMs\":2,\"record\":1,\"pin\":null,\"unknown\":true}}\n")
        .unwrap();
    assert!(matches!(
        journal.record_snapshot(STEM, true),
        Err(JournalError::Corrupt)
    ));
}

#[test]
fn a_pin_survives_restart_without_rewriting_the_file() {
    use dekopon_agent::memory_tools::Rung;
    use std::os::unix::fs::MetadataExt as _;
    let (dir, journal) = journal();
    let mut record = fixture();
    record.plan = None;
    record.summaries.clear();
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    let path = journal.path(STEM);
    let inode = std::fs::metadata(&path).unwrap().ino();
    let original = std::fs::read(&path).unwrap();
    journal
        .write_pin(
            STEM,
            PinLine {
                at_ms: 2,
                record: record.id,
                pin: Some(Rung::Native),
                subject: Subject::Record,
            },
        )
        .unwrap();
    drop(journal);
    let (journal, _boot) = reopen(dir.path());
    assert_eq!(
        journal
            .record_snapshot(STEM, true)
            .unwrap()
            .unwrap()
            .records[0]
            .header
            .pin,
        Some(Rung::Native)
    );
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    assert!(std::fs::read(&path).unwrap().starts_with(&original));
    journal
        .write_pin(
            STEM,
            PinLine {
                at_ms: 3,
                record: record.id,
                pin: None,
                subject: Subject::Record,
            },
        )
        .unwrap();
    drop(journal);
    let (journal, _boot) = reopen(dir.path());
    assert_eq!(
        journal
            .record_snapshot(STEM, true)
            .unwrap()
            .unwrap()
            .records[0]
            .header
            .pin,
        None
    );
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    journal.rewrite_records(STEM, |_| true).unwrap();
    assert_eq!(
        journal
            .record_snapshot(STEM, true)
            .unwrap()
            .unwrap()
            .records[0]
            .header
            .pin,
        None
    );
}

#[test]
fn the_v3_walk_and_rewrite_decode_headers_and_never_bodies() {
    let (dir, journal) = journal();
    let mut record = fixture();
    record.original = vec![Item::User(Kept::new(
        &"x".repeat(2 * 1024 * 1024),
        usize::MAX,
    ))];
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    journal
        .write_slot(
            STEM,
            SlotLine {
                at_ms: 2,
                record: record.id,
                part: NonZeroU8::new(1).unwrap(),
                slot: Slot::Generated("summary".to_owned()),
                subject: Subject::Record,
            },
        )
        .unwrap();
    journal
        .write_choice(
            STEM,
            ChoiceLine {
                at_ms: 3,
                record: record.id,
                choice: dekopon_agent::memory_tools::Rung::Metadata,
            },
        )
        .unwrap();
    journal
        .write_pin(
            STEM,
            PinLine {
                at_ms: 4,
                record: record.id,
                pin: Some(dekopon_agent::memory_tools::Rung::Native),
                subject: Subject::Record,
            },
        )
        .unwrap();
    DECODED_BYTES.with(|bytes| bytes.set(0));
    let before = journal.record_snapshot(STEM, true).unwrap().unwrap();
    let body = read_body(&before, 0);
    journal.rewrite_records(STEM, |_| true).unwrap();
    assert_eq!(
        read_body(&journal.record_snapshot(STEM, true).unwrap().unwrap(), 0),
        body
    );
    drop(journal);
    let (journal, boot) = reopen(dir.path());
    assert_eq!(boot.reset, 0);
    assert_eq!(
        read_body(&journal.record_snapshot(STEM, true).unwrap().unwrap(), 0),
        body
    );
    DECODED_BYTES.with(|bytes| assert!(bytes.get() < 64 * 1024, "decoded {} bytes", bytes.get()));
}

#[test]
fn an_unlocked_header_reader_keeps_its_snapshot_across_a_rewrite() {
    let (_dir, journal) = journal();
    let mut first = fixture();
    first.original = vec![Item::User(Kept::new("removed", 1024))];
    journal.append_record(STEM, &first, Vec::new()).unwrap();
    let mut second = fixture();
    second.id = RecordId(2.try_into().unwrap());
    second.turn = TurnId(2);
    second.original = vec![Item::User(Kept::new("kept", 1024))];
    journal.append_record(STEM, &second, Vec::new()).unwrap();
    let old = std::thread::scope(|scope| {
        let guard = journal.hold_io_for_test();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let reader = Arc::clone(&journal);
        let worker = scope.spawn(move || tx.send(reader.record_snapshot(STEM, true)).unwrap());
        let result = rx.recv_timeout(Duration::from_secs(3));
        drop(guard);
        worker.join().unwrap();
        result
            .expect("header reader does not wait for the writer lock")
            .unwrap()
            .unwrap()
    });
    journal
        .rewrite_records(STEM, |header| header.id == second.id)
        .unwrap();
    let span = &old.records[1].header.spans[0];
    assert_eq!(
        escaped_text(
            &old.file,
            old.records[1].body_at + span.rel_offset,
            span.len,
            1024
        )
        .unwrap(),
        "kept"
    );
    let new = journal.record_snapshot(STEM, true).unwrap().unwrap();
    assert_eq!(new.records.len(), 1);
    assert_eq!(new.records[0].body_at, 0);
    assert_ne!(old.records[1].body_at, 0);
}

#[test]
fn a_v3_boot_resets_v2_and_other_unloadable_files() {
    let (dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    std::fs::write(journal.path(STEM), b"{\"version\":2}\n").unwrap();
    drop(journal);
    let (journal, boot) = reopen(dir.path());
    assert_eq!(boot.reset, 1);
    assert!(boot.tasks.is_empty());
    assert!(journal.record_snapshot(STEM, true).unwrap().is_none());
}

#[test]
fn arrived_item_numbers_count_asset_markers_without_span_entries() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    record.original = vec![
        Item::User(Kept::new("user", 1024)),
        Item::Asset(1.try_into().unwrap()),
        Item::Steer(Kept::new("steer", 1024)),
        Item::Asset(2.try_into().unwrap()),
    ];
    journal
        .append_record(STEM, &record, vec![arrival(1, 1), arrival(3, 2)])
        .unwrap();
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    let head = &snapshot.records[0];
    assert_eq!(
        head.header
            .spans
            .iter()
            .map(|span| span.item)
            .collect::<Vec<_>>(),
        [0, 2]
    );
    let body: serde_json::Value = serde_json::from_slice(&read_body(&snapshot, 0)).unwrap();
    for asset in &head.header.arrived {
        assert_eq!(
            body["b"][asset.item as usize]["asset"],
            u64::from(asset.number)
        );
    }
}

#[test]
fn boot_never_requeues_asset_slots_or_arrival_summaries() {
    let (dir, journal) = journal();
    let mut record = fixture();
    record.summaries[0] = Slot::Running;
    let mut asset = arrival(1, 1);
    asset.summaries.push(Slot::Running);
    journal.append_record(STEM, &record, vec![asset]).unwrap();
    journal
        .write_slot(
            STEM,
            SlotLine {
                at_ms: 2,
                record: record.id,
                part: NonZeroU8::new(1).unwrap(),
                slot: Slot::Running,
                subject: Subject::Asset(1.try_into().unwrap()),
            },
        )
        .unwrap();
    drop(journal);
    let (journal, boot) = reopen(dir.path());
    assert_eq!(boot.requeued, 1);
    assert_eq!(boot.tasks.len(), 1);
    assert_eq!(boot.tasks[0].part.get(), 1);
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    assert_eq!(snapshot.records[0].header.summaries[0], Slot::Queued);
    assert_eq!(
        snapshot.records[0].header.arrived[0].summaries,
        [Slot::Running]
    );
    let [AssetSideLine::Slot(line)] = snapshot.records[0].asset_lines.as_slice() else {
        panic!("asset slot stays unapplied");
    };
    assert_eq!(line.slot, Slot::Running);
}

#[test]
fn a_record_lookup_retains_only_its_header_but_refuses_unknown_side_fields() {
    let (_dir, journal) = journal();
    for number in 1..=3 {
        let mut record = fixture();
        record.id = RecordId(number.try_into().unwrap());
        record.turn = TurnId(number);
        journal.append_record(STEM, &record, Vec::new()).unwrap();
        journal
            .write_slot(
                STEM,
                SlotLine {
                    at_ms: 5,
                    record: record.id,
                    part: NonZeroU8::new(1).unwrap(),
                    slot: Slot::Generated(number.to_string()),
                    subject: Subject::Record,
                },
            )
            .unwrap();
    }
    let record = RecordId(2.try_into().unwrap());
    let snapshot = journal.record_lookup(STEM, record).unwrap().unwrap();
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.records[0].header.id, record);
    assert_eq!(
        snapshot.records[0].header.summaries[0],
        Slot::Generated("2".to_owned())
    );
    assert!(
        journal
            .record_lookup(STEM, RecordId(999.try_into().unwrap()))
            .unwrap()
            .unwrap()
            .records
            .is_empty()
    );
    let mut file = OpenOptions::new()
        .append(true)
        .open(journal.path(STEM))
        .unwrap();
    file.write_all(
        b"{\"s\":{\"atMs\":6,\"record\":3,\"part\":1,\"slot\":\"Queued\",\"unknown\":true}}\n",
    )
    .unwrap();
    assert!(matches!(
        journal.record_lookup(STEM, record),
        Err(JournalError::Corrupt)
    ));
}

#[test]
fn span_kinds_use_the_v3_lowercase_tokens() {
    assert_eq!(
        serde_json::to_string(&Kind::Escaped).unwrap(),
        "\"escaped\""
    );
    assert_eq!(serde_json::to_string(&Kind::Raw).unwrap(), "\"raw\"");
}

#[test]
fn boot_pending_work_is_bounded_and_overflow_finalizes_later_parts() {
    let (dir, journal) = journal();
    for number in 1..=3 {
        let mut record = fixture();
        record.id = RecordId(number.try_into().unwrap());
        record.turn = TurnId(number);
        record.summaries[0] = Slot::Running;
        journal.append_record(STEM, &record, Vec::new()).unwrap();
    }
    drop(journal);
    let (journal, boot) = Journal::open_records(
        dir.path(),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        1.try_into().unwrap(),
    )
    .unwrap();
    assert_eq!(boot.tasks.len(), 1);
    assert_eq!(boot.tasks[0].record.0.get(), 1);
    assert_eq!(boot.requeued, 3);
    assert_eq!(boot.overflowed, 6);
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    assert_eq!(snapshot.records[0].header.summaries, vec![Slot::Queued; 3]);
    assert!(
        snapshot.records[1..]
            .iter()
            .flat_map(|head| &head.header.summaries)
            .all(
                |slot| matches!(slot, Slot::Error(error) if error.kind == SummaryFailure::Overflow)
            )
    );
}

#[test]
fn boot_resets_an_inconsistent_plan_instead_of_leaving_a_loading_marker() {
    let (dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    let mut bytes = read_body(&snapshot, 0);
    let mut header = snapshot.records[0].header.clone();
    header.plan.as_mut().unwrap().parts.clear();
    bytes.extend(serde_json::to_vec(&HeaderLine { h: header }).unwrap());
    bytes.push(b'\n');
    std::fs::write(journal.path(STEM), bytes).unwrap();
    drop(journal);
    let (_journal, boot) = reopen(dir.path());
    assert_eq!(boot.reset, 1);
    assert!(boot.tasks.is_empty());
    assert!(!dir.path().join("loading.marker").exists());
}

#[test]
fn an_unknown_tag_is_refused_before_reading_its_large_payload() {
    use std::io::Seek as _;
    let (_dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    let path = journal.path(STEM);
    let mut out = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    out.write_all(b"{\"version\":2,\"user\":\"").unwrap();
    out.write_all(&vec![b'x'; 2 * 1024 * 1024]).unwrap();
    out.write_all(b"\"}\n").unwrap();
    drop(out);
    let mut probe = File::open(path).unwrap();
    let reader = probe.try_clone().unwrap();
    assert!(matches!(
        scan(reader, true, None),
        Err(JournalError::Corrupt)
    ));
    assert!(probe.stream_position().unwrap() <= BUFFER as u64);
}

#[test]
fn the_backward_window_skips_bodies_and_joins_side_lines_in_file_order() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    record.original = vec![Item::User(Kept::new(
        &"x".repeat(2 * 1024 * 1024),
        usize::MAX,
    ))];
    for id in 1..=4 {
        record.id = RecordId(NonZeroU64::new(id).unwrap());
        journal
            .append_record("agent/hash", &record, Vec::new())
            .unwrap();
    }
    for slot in [Slot::Running, Slot::Generated("latest".to_owned())] {
        journal
            .write_slot(
                "agent/hash",
                SlotLine {
                    at_ms: 1,
                    record: record.id,
                    part: NonZeroU8::new(1).unwrap(),
                    slot,
                    subject: Subject::Record,
                },
            )
            .unwrap();
    }
    for subject in [
        Subject::Asset(1.try_into().unwrap()),
        Subject::Asset(2.try_into().unwrap()),
    ] {
        journal
            .write_slot(
                "agent/hash",
                SlotLine {
                    at_ms: 1,
                    record: record.id,
                    part: NonZeroU8::new(1).unwrap(),
                    slot: Slot::Running,
                    subject,
                },
            )
            .unwrap();
    }
    let _writer = journal.lock.lock();
    DECODED_BYTES.set(0);
    let snapshot = journal
        .record_window("agent/hash", 2.try_into().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot
            .records
            .iter()
            .map(|head| head.header.id.0.get())
            .collect::<Vec<_>>(),
        [3, 4]
    );
    assert_eq!(
        snapshot.records[1].header.summaries[0],
        Slot::Generated("latest".to_owned())
    );
    assert_eq!(snapshot.records[1].asset_lines.len(), 2);
    for (line, expected) in snapshot.records[1].asset_lines.iter().zip([1, 2]) {
        let AssetSideLine::Slot(line) = line else {
            panic!("slot line")
        };
        assert_eq!(line.subject, Subject::Asset(expected.try_into().unwrap()));
    }
    assert!(DECODED_BYTES.get() < 64 * 1024);
}

#[test]
fn a_backward_window_skips_a_torn_last_header_or_body_without_decoding_it() {
    let (_dir, journal) = journal();
    let record = fixture();
    journal
        .append_record("agent/hash", &record, Vec::new())
        .unwrap();
    for tail in [b"{\"b\":[\"torn".as_slice(), b"{\"h\":{\"torn"] {
        let original = std::fs::metadata(journal.path("agent/hash")).unwrap().len();
        let mut file = OpenOptions::new()
            .append(true)
            .open(journal.path("agent/hash"))
            .unwrap();
        file.write_all(tail).unwrap();
        let snapshot = journal
            .record_window("agent/hash", 2.try_into().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.records.len(), 1);
        file.set_len(original).unwrap();
    }
}

#[test]
fn seal_readers_keep_a_whole_inode_and_never_wait_on_the_writer() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let (_dir, journal) = journal();
    let stem = "agent/hash";
    journal
        .seal(stem, UNIX_EPOCH + Duration::from_millis(1))
        .unwrap();
    let mut original = File::open(journal.seal_path(stem)).unwrap();
    journal.seal(stem, SystemTime::now()).unwrap();
    let mut bytes = Vec::new();
    original.read_to_end(&mut bytes).unwrap();
    assert_eq!(
        serde_json::from_slice::<super::super::Seal>(&bytes)
            .unwrap()
            .at_ms,
        1
    );
    let _writer = journal.lock.lock();
    assert!(
        journal
            .sealed(stem, Duration::from_secs(3600))
            .unwrap()
            .is_some()
    );
}

#[test]
fn unknown_nested_span_slot_and_asset_source_fields_are_corrupt() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    record.assets.push(AssetRow {
        id: 1,
        at_ms: 1,
        name: "fixture".to_owned(),
        mime: "text/plain".to_owned(),
        size: Some(1),
        source: Some(crate::asset::AssetSourceRef::Generated {
            capability: "fixture".to_owned(),
            invocation: "fixture".to_owned(),
        }),
    });
    record.next_asset_id = 2;
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    let path = journal.path(STEM);
    let raw = std::fs::read_to_string(&path).unwrap();
    let bad_span = raw.replacen("\"User\":{", "\"User\":{\"futureField\":7,", 1);
    let bad_source = raw.replacen(
        "\"kind\":\"generated\",",
        "\"kind\":\"generated\",\"futureField\":7,",
        1,
    );
    let bad_slot = format!(
        "{raw}{}\n",
        r#"{"s":{"atMs":2,"record":1,"part":1,"slot":{"Error":{"kind":{"Io":{"kind":"NotFound","futureField":1}},"detail":"x"}}}}"#
    );
    for changed in [bad_span, bad_source, bad_slot] {
        assert_ne!(changed, raw);
        std::fs::write(&path, &changed).unwrap();
        assert!(matches!(
            journal.record_snapshot(STEM, true),
            Err(JournalError::Corrupt)
        ));
        assert!(matches!(
            journal.rewrite_records(STEM, |_| true),
            Err(JournalError::Corrupt)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
    }
}

#[test]
fn boot_overflow_preserves_a_finalized_middle_slot_and_counts_only_pending_slots() {
    let (_dir, journal) = journal();
    journal.append_record(STEM, &fixture(), Vec::new()).unwrap();
    let mut second = fixture();
    second.id = RecordId(2.try_into().unwrap());
    second.turn = TurnId(2);
    second.summaries = vec![
        Slot::Running,
        Slot::Generated("kept".to_owned()),
        Slot::Queued,
    ];
    journal.append_record(STEM, &second, Vec::new()).unwrap();
    let boot = journal.load_summary_tasks(1.try_into().unwrap()).unwrap();
    assert_eq!(boot.tasks.len(), 1);
    assert_eq!(boot.overflowed, 2);
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    let slots = &snapshot.records[1].header.summaries;
    assert_eq!(slots[1], Slot::Generated("kept".to_owned()));
    for index in [0, 2] {
        assert!(
            matches!(&slots[index], Slot::Error(error) if error.kind == SummaryFailure::Overflow)
        );
    }
}

#[test]
fn a_pending_chain_error_never_overwrites_a_newly_finalized_slot() {
    let (_dir, journal) = journal();
    let record = fixture();
    journal.append_record(STEM, &record, Vec::new()).unwrap();
    let stale = journal.record_snapshot(STEM, true).unwrap().unwrap();
    let head = &stale.records[0].header;
    let mut line = SlotLine {
        at_ms: 1,
        record: record.id,
        part: 1.try_into().unwrap(),
        slot: Slot::Generated("kept".to_owned()),
        subject: Subject::Record,
    };
    assert!(
        journal
            .write_slot_for(STEM, line.clone(), record.turn)
            .unwrap()
    );
    line.slot = Slot::Error(SummaryError {
        kind: SummaryFailure::Overflow,
        detail: "overflow".to_owned(),
    });
    assert!(
        !journal
            .write_pending_slot_for(STEM, line, head.turn)
            .unwrap()
    );
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    assert_eq!(
        snapshot.records[0].header.summaries[0],
        Slot::Generated("kept".to_owned())
    );
}

#[test]
fn indexed_side_lines_join_the_first_kept_record_after_later_appends_and_a_rewrite() {
    let (_dir, journal) = journal();
    for number in 1..=8 {
        let mut record = fixture();
        record.id = RecordId(number.try_into().unwrap());
        journal.append_record(STEM, &record, Vec::new()).unwrap();
        if number >= 3 {
            journal
                .write_pin(
                    STEM,
                    PinLine {
                        at_ms: number,
                        record: RecordId(3.try_into().unwrap()),
                        pin: Some(dekopon_agent::memory_tools::Rung::Metadata),
                        subject: Subject::Record,
                    },
                )
                .unwrap();
        }
    }
    journal
        .rewrite_records(STEM, |header| header.id.0.get() >= 3)
        .unwrap();
    let line = PinLine {
        at_ms: 9,
        record: RecordId(3.try_into().unwrap()),
        pin: None,
        subject: Subject::Record,
    };
    journal.write_pin(STEM, line).unwrap();
    let snapshot = journal.record_snapshot(STEM, true).unwrap().unwrap();
    assert_eq!(snapshot.records.len(), 6);
    assert!(snapshot.records[0].header.pin.is_none());
    assert!(
        journal
            .record_lookup(STEM, RecordId(3.try_into().unwrap()))
            .unwrap()
            .unwrap()
            .records[0]
            .header
            .pin
            .is_none()
    );
}

#[test]
fn an_unmatched_body_below_a_completed_record_is_corrupt_but_a_trailing_one_is_not() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    let orphan = |journal: &Journal| {
        OpenOptions::new()
            .append(true)
            .open(journal.path("agent/hash"))
            .unwrap()
            .write_all(b"{\"b\":[]}\n")
            .unwrap();
    };
    journal
        .append_record("agent/hash", &record, Vec::new())
        .unwrap();
    orphan(&journal);
    let trailing = journal
        .record_window("agent/hash", 4.try_into().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(trailing.records.len(), 1);
    record.id = RecordId(NonZeroU64::new(2).unwrap());
    journal
        .append_record("agent/hash", &record, Vec::new())
        .unwrap();
    assert!(matches!(
        journal.record_window("agent/hash", 4.try_into().unwrap()),
        Err(JournalError::Corrupt)
    ));
}

#[test]
fn a_side_line_backlog_for_records_outside_the_window_is_streamed_not_retained() {
    let (_dir, journal) = journal();
    let mut record = fixture();
    for id in 1..=3 {
        record.id = RecordId(NonZeroU64::new(id).unwrap());
        journal
            .append_record("agent/hash", &record, Vec::new())
            .unwrap();
    }
    let outside = |asset: u64| SlotLine {
        at_ms: 1,
        record: RecordId(NonZeroU64::new(1).unwrap()),
        part: NonZeroU8::new(1).unwrap(),
        slot: Slot::Running,
        subject: Subject::Asset(asset.try_into().unwrap()),
    };
    journal.write_slot("agent/hash", outside(1)).unwrap();
    let path = journal.path("agent/hash");
    let bytes = std::fs::read(&path).unwrap();
    let start = bytes[..bytes.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .unwrap()
        + 1;
    let line = bytes[start..].to_vec();
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    for _ in 0..5_000 {
        file.write_all(&line).unwrap();
    }
    journal
        .write_slot(
            "agent/hash",
            SlotLine {
                record: record.id,
                ..outside(1)
            },
        )
        .unwrap();
    DECODED_HEADERS.with_borrow_mut(Vec::clear);
    let snapshot = journal
        .record_window("agent/hash", 1.try_into().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.records[0].header.id, record.id);
    assert_eq!(snapshot.records[0].raw_asset_lines.len(), 1);
    DECODED_HEADERS.with_borrow(|kinds| {
        let last_header = kinds.iter().rposition(|header| *header).unwrap();
        let first_side = kinds.iter().position(|header| !header).unwrap();
        assert!(
            last_header < first_side,
            "a side line was held before its window was known"
        );
        assert_eq!(kinds.len() - first_side, 5_002);
    });
}

#[test]
fn only_one_trailing_body_is_an_append_in_flight() {
    let (_dir, journal) = journal();
    journal
        .append_record("agent/hash", &fixture(), Vec::new())
        .unwrap();
    let path = journal.path("agent/hash");
    let original = std::fs::metadata(&path).unwrap().len();
    for tail in [
        b"{\"b\":[]}\n".as_slice(),
        b"{\"b\":[]}\n{\"h\":{\"torn",
        b"{\"b\":[]}\n{\"b\":[]}\n",
        b"{\"b\":[]}\n{\"b\":[]}\n{\"h\":{\"torn",
    ] {
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(tail).unwrap();
        let window = journal.record_window("agent/hash", 4.try_into().unwrap());
        if tail.starts_with(b"{\"b\":[]}\n{\"b\":") {
            assert!(matches!(window, Err(JournalError::Corrupt)));
        } else {
            assert_eq!(window.unwrap().unwrap().records.len(), 1);
        }
        file.set_len(original).unwrap();
    }
}
