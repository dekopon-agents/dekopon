use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    num::{NonZeroU8, NonZeroU64},
    os::unix::fs::{FileExt, OpenOptionsExt},
    sync::Arc,
};

use dekopon_agent::memory::{Item, Kept, Outcome, Reasoning, Tool};
use dekopon_model::model::ModelKey;
use serde::{Deserialize, Serialize};

use super::{Journal, JournalError};
use crate::memory::record::{
    ArrivedAsset, AssetRow, GrantDigest, RecordId, Slot, SlotLine, Subject, SummaryPlan, TurnId,
    TurnRecord,
};

const BUFFER: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Header {
    pub id: RecordId,
    pub next_record_id: NonZeroU64,
    pub turn: TurnId,
    pub at_ms: u64,
    pub grant: GrantDigest,
    pub model: ModelKey,
    pub summaries: Vec<Slot>,
    pub plan: Option<SummaryPlan>,
    pub assets: Vec<AssetRow>,
    pub next_asset_id: u64,
    pub body_len: u64,
    pub spans: Vec<Span>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arrived: Vec<ArrivedAsset>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct Span {
    pub item: u32,
    pub field: Field,
    pub kind: Kind,
    pub rel_offset: u64,
    pub len: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Field {
    User {
        original_bytes: u64,
    },
    Steer {
        original_bytes: u64,
    },
    Assistant {
        original_bytes: u64,
    },
    Plain {
        original_bytes: u64,
    },
    Opaque {
        model: ModelKey,
    },
    CallId,
    RejectedTool,
    Arguments {
        tool: NamedTool,
        original_bytes: u64,
    },
    Result {
        outcome: Outcome,
        original_bytes: u64,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum NamedTool {
    Bash,
    ReadSkill,
    FetchChatAsset,
    ReadMemoryRecord,
    SetMemoryPin,
    ClearMemoryPin,
    InspectAgentConfig,
    SuggestImprovement,
    Wake,
    Rejected,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum Kind {
    Escaped,
    Raw,
}

pub(crate) struct Snapshot {
    pub file: Arc<File>,
    pub records: Vec<Head>,
}

pub(crate) struct Head {
    pub header: Header,
    pub body_at: u64,
    pub asset_lines: Vec<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderLine {
    h: Header,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryLine {
    s: SlotLine,
}

fn corrupt(error: serde_json::Error) -> JournalError {
    error
        .io_error_kind()
        .map_or(JournalError::Corrupt, |kind| JournalError::Io { kind })
}

struct BodyWriter<'a> {
    file: &'a mut File,
    offset: u64,
    spans: Vec<Span>,
}

impl Write for BodyWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.file.write(bytes)?;
        self.offset += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl BodyWriter<'_> {
    fn json(&mut self, value: &(impl Serialize + ?Sized)) -> Result<(), JournalError> {
        serde_json::to_writer(self, value).map_err(|error| {
            error
                .io_error_kind()
                .map_or(JournalError::Corrupt, |kind| JournalError::Io { kind })
        })
    }
    fn text(&mut self, item: u32, field: Field, value: &str) -> Result<(), JournalError> {
        let start = self.offset;
        self.json(value)?;
        self.spans.push(Span {
            item,
            field,
            kind: Kind::Escaped,
            rel_offset: start + 1,
            len: self.offset - start - 2,
        });
        Ok(())
    }
    fn kept(&mut self, item: u32, field: Field, value: &Kept) -> Result<(), JournalError> {
        self.write_all(b"{\"text\":")?;
        self.text(item, field, value.text())?;
        self.write_all(b",\"original_bytes\":")?;
        self.json(&value.original_bytes())?;
        self.write_all(b"}")?;
        Ok(())
    }
    fn item(&mut self, index: u32, item: &Item) -> Result<(), JournalError> {
        match item {
            Item::User(text) => {
                self.write_all(b"{\"user\":")?;
                self.kept(
                    index,
                    Field::User {
                        original_bytes: text.original_bytes(),
                    },
                    text,
                )?;
            }
            Item::Steer(text) => {
                self.write_all(b"{\"steer\":")?;
                self.kept(
                    index,
                    Field::Steer {
                        original_bytes: text.original_bytes(),
                    },
                    text,
                )?;
            }
            Item::Assistant(text) => {
                self.write_all(b"{\"assistant\":")?;
                self.kept(
                    index,
                    Field::Assistant {
                        original_bytes: text.original_bytes(),
                    },
                    text,
                )?;
            }
            Item::Reasoning(Reasoning::Plain(text)) => {
                self.write_all(b"{\"reasoning\":{\"Plain\":")?;
                self.kept(
                    index,
                    Field::Plain {
                        original_bytes: text.original_bytes(),
                    },
                    text,
                )?;
                self.write_all(b"}")?;
            }
            Item::Reasoning(Reasoning::Opaque { model, bytes }) => {
                self.write_all(b"{\"reasoning\":{\"Opaque\":{\"model\":")?;
                self.json(model)?;
                self.write_all(b",\"bytes\":")?;
                let start = self.offset;
                self.write_all(bytes.bytes())?;
                self.spans.push(Span {
                    item: index,
                    field: Field::Opaque {
                        model: model.clone(),
                    },
                    kind: Kind::Raw,
                    rel_offset: start,
                    len: self.offset - start,
                });
                self.write_all(b"}}")?;
            }
            Item::Exchange(exchange) => {
                self.write_all(b"{\"exchange\":{\"call\":{\"id\":")?;
                self.text(index, Field::CallId, exchange.call.id.as_str())?;
                self.write_all(b",\"tool\":")?;
                let tool = match &exchange.call.tool {
                    Tool::Bash => NamedTool::Bash,
                    Tool::ReadSkill => NamedTool::ReadSkill,
                    Tool::FetchChatAsset => NamedTool::FetchChatAsset,
                    Tool::ReadMemoryRecord => NamedTool::ReadMemoryRecord,
                    Tool::SetMemoryPin => NamedTool::SetMemoryPin,
                    Tool::ClearMemoryPin => NamedTool::ClearMemoryPin,
                    Tool::InspectAgentConfig => NamedTool::InspectAgentConfig,
                    Tool::SuggestImprovement => NamedTool::SuggestImprovement,
                    Tool::Wake => NamedTool::Wake,
                    Tool::Rejected(name) => {
                        self.write_all(b"{\"Rejected\":")?;
                        self.kept(index, Field::RejectedTool, name)?;
                        self.write_all(b"}")?;
                        NamedTool::Rejected
                    }
                };
                if !matches!(tool, NamedTool::Rejected) {
                    self.json(&exchange.call.tool)?;
                }
                self.write_all(b",\"arguments\":")?;
                self.kept(
                    index,
                    Field::Arguments {
                        tool,
                        original_bytes: exchange.call.arguments.original_bytes(),
                    },
                    &exchange.call.arguments,
                )?;
                self.write_all(b"},\"result\":{\"outcome\":")?;
                self.json(&exchange.result.outcome)?;
                self.write_all(b",\"text\":")?;
                self.kept(
                    index,
                    Field::Result {
                        outcome: exchange.result.outcome,
                        original_bytes: exchange.result.text.original_bytes(),
                    },
                    &exchange.result.text,
                )?;
                self.write_all(b"}}")?;
            }
            Item::Asset(number) => {
                self.write_all(b"{\"asset\":")?;
                self.json(number)?;
            }
        }
        self.write_all(b"}")?;
        Ok(())
    }
}

impl Journal {
    pub(crate) fn append_record(
        &self,
        stem: &str,
        record: &TurnRecord,
        arrived: Vec<ArrivedAsset>,
    ) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        use std::os::unix::fs::DirBuilderExt as _;
        let path = self.path(stem);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path.parent().ok_or(JournalError::Corrupt)?)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.path(stem))?;
        let mut body = BodyWriter {
            file: &mut file,
            offset: 0,
            spans: Vec::new(),
        };
        body.write_all(b"{\"b\":[")?;
        for (index, item) in record.original.iter().enumerate() {
            if index > 0 {
                body.write_all(b",")?;
            }
            body.item(
                u32::try_from(index).map_err(|_| JournalError::Corrupt)?,
                item,
            )?;
        }
        body.write_all(b"]}\n")?;
        let header = Header {
            id: record.id,
            next_record_id: NonZeroU64::new(
                record
                    .id
                    .0
                    .get()
                    .checked_add(1)
                    .ok_or(JournalError::Corrupt)?,
            )
            .ok_or(JournalError::Corrupt)?,
            turn: record.turn,
            at_ms: record.at_ms,
            grant: record.grant.clone(),
            model: record.model.clone(),
            summaries: record.summaries.clone(),
            plan: record.plan.clone(),
            assets: record.assets.clone(),
            next_asset_id: record.next_asset_id,
            body_len: body.offset,
            spans: body.spans,
            arrived,
        };
        serde_json::to_writer(&mut file, &HeaderLine { h: header }).map_err(corrupt)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    pub(crate) fn record_snapshot(
        &self,
        stem: &str,
        strict: bool,
    ) -> Result<Option<Snapshot>, JournalError> {
        let file = match File::open(self.path(stem)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        scan(file, strict).map(Some)
    }

    pub(crate) fn write_slot(&self, stem: &str, line: SlotLine) -> Result<bool, JournalError> {
        let _lock = self.lock.lock();
        let Some(snapshot) = self.record_snapshot(stem, false)? else {
            return Ok(false);
        };
        if !snapshot
            .records
            .iter()
            .any(|head| head.header.id == line.record)
        {
            return Ok(false);
        }
        let mut file = OpenOptions::new().append(true).open(self.path(stem))?;
        serde_json::to_writer(&mut file, &SummaryLine { s: line }).map_err(corrupt)?;
        file.write_all(b"\n")?;
        Ok(true)
    }
}

pub(crate) fn scan(file: File, strict: bool) -> Result<Snapshot, JournalError> {
    let end = file.metadata()?.len();
    let file = Arc::new(file);
    let mut reader = BufReader::with_capacity(BUFFER, file.as_ref());
    let mut offset = 0_u64;
    let mut body = None;
    let mut records: Vec<Head> = Vec::new();
    let mut line = Vec::new();
    while offset < end {
        let at = offset;
        line.clear();
        let mut is_body = false;
        loop {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                break;
            }
            let available = buffer
                .len()
                .min(usize::try_from(end - offset).unwrap_or(usize::MAX));
            let buffer = &buffer[..available];
            let count = buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(buffer.len(), |n| n + 1);
            if line.len() < 5 {
                line.extend_from_slice(&buffer[..count.min(5 - line.len())]);
            }
            if line.len() == 5 && line == b"{\"b\":" {
                is_body = true;
            }
            if !is_body {
                let already = usize::try_from(offset - at).unwrap_or(usize::MAX);
                let skip = line.len().saturating_sub(already).min(count);
                line.extend_from_slice(&buffer[skip..count]);
            }
            let newline = buffer[count - 1] == b'\n';
            reader.consume(count);
            offset += count as u64;
            if newline || offset == end {
                break;
            }
        }
        let mut last = [0];
        if offset == at {
            return Err(JournalError::Corrupt);
        }
        file.read_exact_at(&mut last, offset - 1)?;
        if last[0] != b'\n' {
            if strict {
                return Err(JournalError::Corrupt);
            }
            break;
        }
        if is_body {
            if body.replace((at, offset - at)).is_some() {
                return Err(JournalError::Corrupt);
            }
            continue;
        }
        if line.starts_with(b"{\"h\":") {
            let header = serde_json::from_slice::<HeaderLine>(&line)
                .map_err(corrupt)?
                .h;
            let (body_at, body_len) = body.take().ok_or(JournalError::Corrupt)?;
            if header.body_len != body_len
                || header.spans.iter().any(|span| {
                    span.rel_offset
                        .checked_add(span.len)
                        .is_none_or(|end| end > body_len)
                })
            {
                return Err(JournalError::Corrupt);
            }
            if records
                .last()
                .is_some_and(|previous| previous.header.next_record_id != header.id.0)
            {
                return Err(JournalError::Corrupt);
            }
            records.push(Head {
                header,
                body_at,
                asset_lines: Vec::new(),
            });
        } else if line.starts_with(b"{\"s\":") && body.is_none() {
            let slot = serde_json::from_slice::<SummaryLine>(&line)
                .map_err(corrupt)?
                .s;
            let head = records
                .iter_mut()
                .rev()
                .find(|head| head.header.id == slot.record)
                .ok_or(JournalError::Corrupt)?;
            match slot.subject {
                Subject::Record => {
                    let part = usize::from(slot.part.get() - 1);
                    *head
                        .header
                        .summaries
                        .get_mut(part)
                        .ok_or(JournalError::Corrupt)? = slot.slot;
                }
                Subject::Asset(_) => head.asset_lines.push(line.clone()),
            }
        } else {
            return Err(JournalError::Corrupt);
        }
    }
    if strict && body.is_some() {
        return Err(JournalError::Corrupt);
    }
    Ok(Snapshot { file, records })
}

pub(crate) fn escaped_text(
    file: &File,
    offset: u64,
    len: u64,
    cap: usize,
) -> Result<String, JournalError> {
    let mut reader = BufReader::with_capacity(
        BUFFER,
        SliceReader {
            file,
            offset,
            left: len,
        },
    );
    let mut left = len;
    let mut text = String::with_capacity(cap.min(usize::try_from(len).unwrap_or(usize::MAX)));
    while left > 0 && text.len() < cap {
        let mut atom = [0_u8; 14];
        atom[0] = b'"';
        read_atom_byte(&mut reader, &mut left, &mut atom[1])?;
        let n = match atom[1] {
            b'\\' => {
                read_atom_byte(&mut reader, &mut left, &mut atom[2])?;
                if atom[2] == b'u' {
                    for byte in &mut atom[3..7] {
                        read_atom_byte(&mut reader, &mut left, byte)?;
                    }
                    let code = u16::from_str_radix(
                        std::str::from_utf8(&atom[3..7]).map_err(|_| JournalError::Corrupt)?,
                        16,
                    )
                    .map_err(|_| JournalError::Corrupt)?;
                    if (0xd800..=0xdbff).contains(&code) {
                        for byte in &mut atom[7..13] {
                            read_atom_byte(&mut reader, &mut left, byte)?;
                        }
                        13
                    } else {
                        7
                    }
                } else {
                    3
                }
            }
            0..=0x7f => 2,
            0xc2..=0xdf => {
                read_atom_byte(&mut reader, &mut left, &mut atom[2])?;
                3
            }
            0xe0..=0xef => {
                for byte in &mut atom[2..4] {
                    read_atom_byte(&mut reader, &mut left, byte)?;
                }
                4
            }
            0xf0..=0xf4 => {
                for byte in &mut atom[2..5] {
                    read_atom_byte(&mut reader, &mut left, byte)?;
                }
                5
            }
            0x80..=0xc1 | 0xf5..=0xff => return Err(JournalError::Corrupt),
        };
        atom[n] = b'"';
        let value: String = serde_json::from_slice(&atom[..=n]).map_err(corrupt)?;
        if text.len() + value.len() > cap {
            break;
        }
        text.push_str(&value);
    }
    Ok(text)
}

fn read_atom_byte(
    reader: &mut impl Read,
    left: &mut u64,
    out: &mut u8,
) -> Result<(), JournalError> {
    if *left == 0 {
        return Err(JournalError::Corrupt);
    }
    reader.read_exact(std::slice::from_mut(out))?;
    *left -= 1;
    Ok(())
}

struct SliceReader<'a> {
    file: &'a File,
    offset: u64,
    left: u64,
}
impl Read for SliceReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let len = out
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.file.read_at(&mut out[..len], self.offset)?;
        self.offset += n as u64;
        self.left -= n as u64;
        Ok(n)
    }
}

#[cfg(test)]
pub(crate) mod tests {
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
        let journal = Arc::new(
            Journal::open(
                dir.path(),
                Duration::from_secs(3600),
                Duration::from_secs(3600),
            )
            .unwrap(),
        );
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
}

#[derive(Default)]
pub(crate) struct Boot {
    pub reset: u32,
    pub requeued: u32,
    pub crashed_files: u32,
    pub tasks: Vec<crate::memory::engine::SlotTask>,
}

impl Journal {
    pub(crate) fn load_summary_tasks(&self) -> Result<Boot, JournalError> {
        use std::fs;
        let _lock = self.lock.lock();
        let mut boot = Boot::default();
        let marker = self.dir.join("loading.marker");
        if marker.exists() {
            if fs::metadata(&marker)?.len() <= 4096 {
                let previous = fs::read_to_string(&marker)?;
                if valid_stem(&previous) {
                    let path = self.path(&previous);
                    if path.exists() {
                        fs::remove_file(path)?;
                        boot.reset += 1;
                    }
                    boot.crashed_files += 1;
                }
            }
            fs::remove_file(&marker)?;
        }
        let mut agents = fs::read_dir(&self.dir)?.collect::<Result<Vec<_>, _>>()?;
        agents.sort_by_key(std::fs::DirEntry::file_name);
        for agent in agents {
            if !agent.file_type()?.is_dir() {
                continue;
            }
            let name = agent.file_name();
            let Some(name) = name
                .to_str()
                .filter(|name| name.parse::<dekopon_core::AgentId>().is_ok())
            else {
                continue;
            };
            let mut files = fs::read_dir(agent.path())?.collect::<Result<Vec<_>, _>>()?;
            files.sort_by_key(std::fs::DirEntry::file_name);
            for file in files {
                if !file.file_type()?.is_file() {
                    continue;
                }
                let path = file.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(hash) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                let stem = format!("{name}/{hash}");
                if !valid_stem(&stem) {
                    continue;
                }
                let mut marker_file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&marker)?;
                marker_file.write_all(stem.as_bytes())?;
                drop(marker_file);
                let loaded = self.record_snapshot(&stem, true);
                match loaded {
                    Ok(Some(snapshot)) => {
                        let mut append = OpenOptions::new().append(true).open(&path)?;
                        for head in snapshot.records {
                            let mut lowest = None;
                            for (part, slot) in head.header.summaries.iter().enumerate() {
                                let part = NonZeroU8::new(
                                    u8::try_from(part + 1).map_err(|_| JournalError::Corrupt)?,
                                )
                                .ok_or(JournalError::Corrupt)?;
                                match slot {
                                    Slot::Running => {
                                        let line = SlotLine {
                                            at_ms: super::millis(std::time::SystemTime::now()),
                                            record: head.header.id,
                                            part,
                                            slot: Slot::Queued,
                                            subject: Subject::Record,
                                        };
                                        serde_json::to_writer(
                                            &mut append,
                                            &SummaryLine { s: line },
                                        )
                                        .map_err(corrupt)?;
                                        append.write_all(b"\n")?;
                                        boot.requeued += 1;
                                        lowest.get_or_insert(part);
                                    }
                                    Slot::Queued => {
                                        lowest.get_or_insert(part);
                                    }
                                    Slot::Generated(_) | Slot::Error(_) => {}
                                }
                            }
                            if let Some(part) = lowest {
                                boot.tasks.push(crate::memory::engine::SlotTask {
                                    stem: stem.clone(),
                                    record: head.header.id,
                                    part,
                                });
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(_unloadable) => {
                        fs::remove_file(&path)?;
                        boot.reset += 1;
                    }
                }
                fs::remove_file(&marker)?;
            }
        }
        Ok(boot)
    }
}

fn valid_stem(stem: &str) -> bool {
    stem.split_once('/').is_some_and(|(agent, hash)| {
        agent.parse::<dekopon_core::AgentId>().is_ok()
            && hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}
