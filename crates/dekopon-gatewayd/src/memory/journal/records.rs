pub(crate) mod walk;

use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    num::{NonZeroU8, NonZeroU64, NonZeroUsize},
    os::unix::fs::{FileExt, OpenOptionsExt},
    sync::Arc,
};

use dekopon_agent::memory::{Item, Kept, Outcome, Reasoning, Tool};
use dekopon_model::model::ModelKey;
use serde::{Deserialize, Serialize};

use super::{Journal, JournalError};
use crate::memory::record::{
    ArrivedAsset, AssetRow, AssetSideLine, ChoiceLine, GrantDigest, PinLine, RecordId, Slot,
    SlotLine, Subject, SummaryError, SummaryFailure, SummaryPlan, TurnId, TurnRecord,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<dekopon_agent::memory_tools::Rung>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<dekopon_agent::memory_tools::Rung>,
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
#[serde(deny_unknown_fields)]
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

impl NamedTool {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::ReadSkill => "read_skill",
            Self::FetchChatAsset => "fetch_chat_asset",
            Self::ReadMemoryRecord => "read_memory_record",
            Self::SetMemoryPin => "set_memory_pin",
            Self::ClearMemoryPin => "clear_memory_pin",
            Self::InspectAgentConfig => "inspect_agent_config",
            Self::SuggestImprovement => "suggest_improvement",
            Self::Wake => "wake",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    pub asset_lines: Vec<AssetSideLine>,
    raw_asset_lines: Vec<Vec<u8>>,
    arrived_raw: Option<Box<serde_json::value::RawValue>>,
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinEnvelope {
    p: PinLine,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChoiceEnvelope {
    c: ChoiceLine,
}

#[derive(Deserialize)]
struct RawArrivals {
    #[serde(default)]
    arrived: Option<Box<serde_json::value::RawValue>>,
}
#[derive(Deserialize)]
struct RawHeaderLine {
    h: RawArrivals,
}

#[cfg(test)]
thread_local! {
    pub(super) static DECODED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static DECODED_HEADERS: std::cell::RefCell<Vec<bool>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn decode_line<T: serde::de::DeserializeOwned>(line: &[u8]) -> Result<T, JournalError> {
    #[cfg(test)]
    {
        DECODED_BYTES.with(|bytes| bytes.set(bytes.get() + line.len()));
        DECODED_HEADERS.with_borrow_mut(|kinds| kinds.push(line.starts_with(b"{\"h\":")));
    }
    serde_json::from_slice(line).map_err(corrupt)
}

enum Side {
    Slot(SlotLine),
    Pin(PinLine),
    Choice(ChoiceLine),
}
impl Side {
    fn decode(line: &[u8]) -> Result<Self, JournalError> {
        if line.starts_with(b"{\"s\":") {
            Ok(Self::Slot(decode_line::<SummaryLine>(line)?.s))
        } else if line.starts_with(b"{\"p\":") {
            Ok(Self::Pin(decode_line::<PinEnvelope>(line)?.p))
        } else if line.starts_with(b"{\"c\":") {
            Ok(Self::Choice(decode_line::<ChoiceEnvelope>(line)?.c))
        } else {
            Err(JournalError::Corrupt)
        }
    }
    fn record(&self) -> RecordId {
        match self {
            Self::Slot(line) => line.record,
            Self::Pin(line) => line.record,
            Self::Choice(line) => line.record,
        }
    }
    fn apply(self, head: &mut Head, raw: &[u8]) -> Result<(), JournalError> {
        match self {
            Self::Slot(line) => match line.subject {
                Subject::Record => {
                    *head
                        .header
                        .summaries
                        .get_mut(usize::from(line.part.get() - 1))
                        .ok_or(JournalError::Corrupt)? = line.slot
                }
                Subject::Asset(_) => {
                    head.asset_lines.push(AssetSideLine::Slot(line));
                    head.raw_asset_lines.push(raw.to_vec());
                }
            },
            Self::Pin(line) => match line.subject {
                Subject::Record => head.header.pin = line.pin,
                Subject::Asset(_) => {
                    head.asset_lines.push(AssetSideLine::Pin(line));
                    head.raw_asset_lines.push(raw.to_vec());
                }
            },
            Self::Choice(line) => head.header.choice = Some(line.choice),
        }
        Ok(())
    }
}

fn corrupt(error: serde_json::Error) -> JournalError {
    error
        .io_error_kind()
        .map_or(JournalError::Corrupt, |kind| JournalError::Io { kind })
}

struct BodyWriter<W> {
    out: W,
    offset: u64,
    spans: Vec<Span>,
}

impl<W: Write> Write for BodyWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.out.write(bytes)?;
        self.offset += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

impl<W: Write> BodyWriter<W> {
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

pub(super) struct Counted(pub u64);

impl Write for Counted {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode_record(
    out: &mut impl Write,
    record: &TurnRecord,
    arrived: Vec<ArrivedAsset>,
) -> Result<(), JournalError> {
    let next_record_id = NonZeroU64::new(
        record
            .id
            .0
            .get()
            .checked_add(1)
            .ok_or(JournalError::Corrupt)?,
    )
    .ok_or(JournalError::Corrupt)?;
    u32::try_from(record.original.len()).map_err(|_invalid| JournalError::Corrupt)?;
    let mut body = BodyWriter {
        out: &mut *out,
        offset: 0,
        spans: Vec::new(),
    };
    body.write_all(b"{\"b\":[")?;
    for (index, item) in record.original.iter().enumerate() {
        if index > 0 {
            body.write_all(b",")?;
        }
        body.item(
            u32::try_from(index).map_err(|_invalid| JournalError::Corrupt)?,
            item,
        )?;
    }
    body.write_all(b"]}\n")?;
    let header = Header {
        id: record.id,
        next_record_id,
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
        pin: None,
        choice: None,
    };
    serde_json::to_writer(&mut *out, &HeaderLine { h: header }).map_err(corrupt)?;
    out.write_all(b"\n")?;
    Ok(())
}

impl Journal {
    #[cfg(test)]
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
        encode_record(&mut file, record, arrived)
    }

    pub(crate) fn record_snapshot(
        &self,
        stem: &str,
        strict: bool,
    ) -> Result<Option<Snapshot>, JournalError> {
        self.record_view(stem, strict, None)
    }

    pub(crate) fn record_lookup(
        &self,
        stem: &str,
        record: RecordId,
    ) -> Result<Option<Snapshot>, JournalError> {
        self.record_view(stem, false, Some(record))
    }

    fn record_view(
        &self,
        stem: &str,
        strict: bool,
        wanted: Option<RecordId>,
    ) -> Result<Option<Snapshot>, JournalError> {
        let file = match File::open(self.path(stem)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        scan(file, strict, wanted).map(Some)
    }

    pub(crate) fn write_slot(&self, stem: &str, line: SlotLine) -> Result<bool, JournalError> {
        self.write_record_side(stem, line.record, None, None, &SummaryLine { s: line })
    }

    pub(crate) fn write_slot_for(
        &self,
        stem: &str,
        line: SlotLine,
        turn: TurnId,
    ) -> Result<bool, JournalError> {
        self.write_record_side(
            stem,
            line.record,
            Some(turn),
            None,
            &SummaryLine { s: line },
        )
    }

    pub(crate) fn write_pending_slot_for(
        &self,
        stem: &str,
        line: SlotLine,
        turn: TurnId,
    ) -> Result<bool, JournalError> {
        self.write_record_side(
            stem,
            line.record,
            Some(turn),
            Some(line.part),
            &SummaryLine { s: line },
        )
    }

    pub(crate) fn write_pin(&self, stem: &str, line: PinLine) -> Result<bool, JournalError> {
        self.write_record_side(stem, line.record, None, None, &PinEnvelope { p: line })
    }

    pub(crate) fn write_choice(&self, stem: &str, line: ChoiceLine) -> Result<bool, JournalError> {
        self.write_record_side(stem, line.record, None, None, &ChoiceEnvelope { c: line })
    }

    fn write_record_side(
        &self,
        stem: &str,
        record: RecordId,
        turn: Option<TurnId>,
        pending: Option<NonZeroU8>,
        line: &impl Serialize,
    ) -> Result<bool, JournalError> {
        let mut bytes = serde_json::to_vec(line).map_err(corrupt)?;
        bytes.push(b'\n');
        let _lock = self.lock.lock();
        let Some(snapshot) = self.record_lookup(stem, record)? else {
            return Ok(false);
        };
        let Some(head) = snapshot.records.first().filter(|head| {
            head.header.id == record && turn.is_none_or(|turn| head.header.turn == turn)
        }) else {
            return Ok(false);
        };
        if pending.is_some_and(|part| {
            !head
                .header
                .summaries
                .get(usize::from(part.get() - 1))
                .is_some_and(Slot::is_pending)
        }) {
            return Ok(false);
        }
        OpenOptions::new()
            .append(true)
            .open(self.path(stem))?
            .write_all(&bytes)?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn rewrite_records(
        &self,
        stem: &str,
        keep: impl FnMut(&Header) -> bool,
    ) -> Result<(), JournalError> {
        let _lock = self.lock.lock();
        rewrite_path(&self.path(stem), keep)
    }
}

pub(super) fn rewrite_path(
    path: &std::path::Path,
    mut keep: impl FnMut(&Header) -> bool,
) -> Result<(), JournalError> {
    {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let snapshot = scan(file, true, None)?;
        let path = path.to_path_buf();
        let temporary = path.with_extension("tmp");
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let result = (|| {
            let mut out = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            for mut head in snapshot.records {
                if !keep(&head.header) {
                    continue;
                }
                let mut body = SliceReader {
                    file: &snapshot.file,
                    offset: head.body_at,
                    left: head.header.body_len,
                };
                if io::copy(&mut body, &mut out)? != head.header.body_len {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
                }
                head.header.arrived.clear();
                let mut header = serde_json::to_vec(&head.header).map_err(corrupt)?;
                header.pop();
                if let Some(arrived) = head.arrived_raw {
                    header.extend_from_slice(b",\"arrived\":");
                    header.extend_from_slice(arrived.get().as_bytes());
                }
                header.push(b'}');
                out.write_all(b"{\"h\":")?;
                out.write_all(&header)?;
                out.write_all(b"}\n")?;
                for line in head.raw_asset_lines {
                    out.write_all(&line)?;
                }
            }
            out.sync_all()?;
            std::fs::rename(&temporary, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _removed = std::fs::remove_file(temporary);
        }
        result
    }
}

fn validate_header(header: &Header, body_len: u64) -> Result<(), JournalError> {
    let valid_plan = match &header.plan {
        Some(plan) => {
            !plan.parts.is_empty()
                && plan.parts.len() <= usize::from(u8::MAX)
                && plan.parts.len() == header.summaries.len()
        }
        None => header.summaries.is_empty(),
    };
    if !valid_plan
        || header.body_len != body_len
        || body_len < 8
        || header.spans.iter().any(|span| {
            span.rel_offset
                .checked_add(span.len)
                .is_none_or(|end| end > body_len)
        })
        || header.id.0.get().checked_add(1) != Some(header.next_record_id.get())
    {
        return Err(JournalError::Corrupt);
    }
    Ok(())
}

impl Journal {
    #[cfg(test)]
    pub(crate) fn record_window(
        &self,
        stem: &str,
        limit: NonZeroUsize,
    ) -> Result<Option<Snapshot>, JournalError> {
        let file = match File::open(self.path(stem)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        walk::window(file, limit).map(Some)
    }
}

fn scan(file: File, strict: bool, wanted: Option<RecordId>) -> Result<Snapshot, JournalError> {
    let end = file.metadata()?.len();
    let file = Arc::new(file);
    let mut reader = BufReader::with_capacity(BUFFER, file.as_ref());
    let mut offset = 0_u64;
    let mut body = None;
    let mut first_id: Option<RecordId> = None;
    let mut next_id: Option<NonZeroU64> = None;
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
            if line.len() == 5 {
                if line == b"{\"b\":" {
                    is_body = true;
                } else if !matches!(
                    line.as_slice(),
                    b"{\"h\":" | b"{\"s\":" | b"{\"c\":" | b"{\"p\":"
                ) {
                    return Err(JournalError::Corrupt);
                }
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
            let header = decode_line::<HeaderLine>(&line)?.h;
            let (body_at, body_len) = body.take().ok_or(JournalError::Corrupt)?;
            validate_header(&header, body_len)?;
            if next_id.is_some_and(|next| next != header.id.0) {
                return Err(JournalError::Corrupt);
            }
            first_id.get_or_insert(header.id);
            next_id = Some(header.next_record_id);
            if wanted.is_some_and(|wanted| wanted != header.id) {
                continue;
            }
            let arrived_raw = decode_line::<RawHeaderLine>(&line)?.h.arrived;
            records.push(Head {
                header,
                body_at,
                asset_lines: Vec::new(),
                raw_asset_lines: Vec::new(),
                arrived_raw,
            });
        } else {
            if body.is_some() {
                return Err(JournalError::Corrupt);
            }
            let side = Side::decode(&line)?;
            let record = side.record();
            if first_id.is_none_or(|first| record < first)
                || next_id.is_none_or(|next| record.0 >= next)
            {
                return Err(JournalError::Corrupt);
            }
            if wanted.is_some_and(|wanted| wanted != record) {
                continue;
            }
            let first = records.first().ok_or(JournalError::Corrupt)?.header.id;
            let index = record
                .0
                .get()
                .checked_sub(first.0.get())
                .and_then(|index| usize::try_from(index).ok())
                .ok_or(JournalError::Corrupt)?;
            let head = records.get_mut(index).ok_or(JournalError::Corrupt)?;
            side.apply(head, &line)?;
        }
    }
    if strict && body.is_some() {
        return Err(JournalError::Corrupt);
    }
    Ok(Snapshot { file, records })
}

pub(crate) fn scan_path(path: &std::path::Path) -> Result<Snapshot, JournalError> {
    scan(File::open(path)?, true, None)
}

pub(crate) fn first_header(path: &std::path::Path) -> Result<Option<Header>, JournalError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut reader = BufReader::with_capacity(BUFFER, file);
    let mut tag = [0; 5];
    match reader.read_exact(&mut tag) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    if &tag != b"{\"b\":" {
        return Err(JournalError::Corrupt);
    }
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(None);
        }
        if let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
            reader.consume(index + 1);
            break;
        }
        let len = buffer.len();
        reader.consume(len);
    }
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Ok(None);
    }
    if !line.starts_with(b"{\"h\":") {
        return Err(JournalError::Corrupt);
    }
    Ok(Some(decode_line::<HeaderLine>(&line)?.h))
}

pub(crate) fn admits(
    header: &Header,
    grant: &GrantDigest,
    horizon_ms: u64,
    sealed_at_ms: Option<u64>,
) -> bool {
    header.grant == *grant
        && header.at_ms >= horizon_ms
        && sealed_at_ms.is_none_or(|seal| header.at_ms > seal)
}

pub(crate) struct RecordText {
    pub total: u64,
    pub selected: Vec<u8>,
}

struct Window {
    total: u64,
    range: Option<(u64, u64)>,
    selected: Vec<u8>,
}

impl Window {
    fn push(&mut self, bytes: &[u8]) {
        let from = self.total;
        self.total += bytes.len() as u64;
        let Some((start, end)) = self.range else {
            return;
        };
        let lo = start.max(from);
        let hi = end.min(self.total);
        if lo < hi {
            let lo = usize::try_from(lo - from).unwrap_or(usize::MAX);
            let hi = usize::try_from(hi - from).unwrap_or(usize::MAX);
            self.selected.extend_from_slice(&bytes[lo..hi]);
        }
    }
}

/// The record's original as one text: each text item on its own line, and each exchange as a
/// tool line followed by its request and result. Opaque reasoning is never read.
pub(crate) fn record_text(
    file: &File,
    head: &Head,
    range: Option<(u64, u64)>,
) -> Result<RecordText, JournalError> {
    let mut window = Window {
        total: 0,
        range,
        selected: Vec::new(),
    };
    let mut buffer = vec![0; BUFFER];
    let mut first = true;
    let mut tool: Option<String> = None;
    for span in &head.header.spans {
        let offset = head
            .body_at
            .checked_add(span.rel_offset)
            .ok_or(JournalError::Corrupt)?;
        match &span.field {
            Field::Opaque { .. } | Field::CallId => continue,
            Field::RejectedTool => {
                let mut name = Vec::new();
                decode_escaped(file, offset, span.len, &mut buffer, &mut |bytes| {
                    name.extend_from_slice(bytes);
                })?;
                tool = Some(String::from_utf8_lossy(&name).into_owned());
                continue;
            }
            Field::User { .. }
            | Field::Steer { .. }
            | Field::Assistant { .. }
            | Field::Plain { .. } => {
                if !first {
                    window.push(b"\n");
                }
            }
            Field::Arguments { tool: named, .. } => {
                if !first {
                    window.push(b"\n");
                }
                let name = match named {
                    NamedTool::Rejected => tool.take().unwrap_or_default(),
                    named @ (NamedTool::Bash
                    | NamedTool::ReadSkill
                    | NamedTool::FetchChatAsset
                    | NamedTool::ReadMemoryRecord
                    | NamedTool::SetMemoryPin
                    | NamedTool::ClearMemoryPin
                    | NamedTool::InspectAgentConfig
                    | NamedTool::SuggestImprovement
                    | NamedTool::Wake) => named.name().to_owned(),
                };
                let outcome = head
                    .header
                    .spans
                    .iter()
                    .find_map(|other| match other.field {
                        Field::Result { outcome, .. } if other.item == span.item => Some(outcome),
                        Field::User { .. }
                        | Field::Steer { .. }
                        | Field::Assistant { .. }
                        | Field::Plain { .. }
                        | Field::Opaque { .. }
                        | Field::CallId
                        | Field::RejectedTool
                        | Field::Arguments { .. }
                        | Field::Result { .. } => None,
                    });
                let outcome = match outcome {
                    Some(Outcome::Exit(code)) => format!("exit {code}"),
                    Some(Outcome::Refused) => "refused".to_owned(),
                    Some(Outcome::Failed) | None => "failed".to_owned(),
                };
                window.push(format!("[tool: {name}, outcome: {outcome}]\n[request] ").as_bytes());
            }
            Field::Result { .. } => window.push(b"\n[result] "),
        }
        first = false;
        decode_escaped(file, offset, span.len, &mut buffer, &mut |bytes| {
            window.push(bytes)
        })?;
    }
    Ok(RecordText {
        total: window.total,
        selected: window.selected,
    })
}

/// Streams a stored JSON string's escaped bytes as their decoded UTF-8, through one buffer.
pub(crate) fn decode_escaped(
    file: &File,
    offset: u64,
    len: u64,
    buffer: &mut [u8],
    sink: &mut dyn FnMut(&[u8]),
) -> Result<(), JournalError> {
    let mut at = 0_u64;
    let mut unescaper = Unescaper::default();
    while at < len {
        let count = usize::try_from((len - at).min(buffer.len() as u64)).unwrap_or(buffer.len());
        let chunk = &mut buffer[..count];
        file.read_exact_at(chunk, offset + at)?;
        at += count as u64;
        unescaper.feed(chunk, sink)?;
    }
    unescaper.finish()
}

#[derive(Default)]
struct Unescaper {
    pending: Vec<u8>,
    high: Option<u16>,
}

impl Unescaper {
    fn feed(&mut self, chunk: &[u8], sink: &mut dyn FnMut(&[u8])) -> Result<(), JournalError> {
        let Self { pending, high } = self;
        let mut run = 0;
        let mut index = 0;
        while index < chunk.len() {
            if pending.is_empty() && chunk[index] != b'\\' {
                index += 1;
                continue;
            }
            if pending.is_empty() {
                sink(&chunk[run..index]);
            }
            pending.push(chunk[index]);
            index += 1;
            run = index;
            let complete = match pending.get(1) {
                None => false,
                Some(b'u') => pending.len() == 6,
                Some(_) => true,
            };
            if !complete {
                continue;
            }
            let decoded: Option<char> = match pending[1] {
                b'"' => Some('"'),
                b'\\' => Some('\\'),
                b'/' => Some('/'),
                b'b' => Some('\u{8}'),
                b'f' => Some('\u{c}'),
                b'n' => Some('\n'),
                b'r' => Some('\r'),
                b't' => Some('\t'),
                b'u' => {
                    let code = std::str::from_utf8(&pending[2..6])
                        .ok()
                        .and_then(|hex| u16::from_str_radix(hex, 16).ok())
                        .ok_or(JournalError::Corrupt)?;
                    match (high.take(), code) {
                        (None, 0xd800..=0xdbff) => {
                            *high = Some(code);
                            None
                        }
                        (Some(high), 0xdc00..=0xdfff) => char::from_u32(
                            0x10000
                                + ((u32::from(high) - 0xd800) << 10)
                                + (u32::from(code) - 0xdc00),
                        ),
                        (None, code) => char::from_u32(u32::from(code)),
                        (Some(_), _) => return Err(JournalError::Corrupt),
                    }
                    .map_or_else(
                        || {
                            if high.is_some() {
                                Ok(None)
                            } else {
                                Err(JournalError::Corrupt)
                            }
                        },
                        |character| Ok(Some(character)),
                    )?
                }
                _ => return Err(JournalError::Corrupt),
            };
            pending.clear();
            if let Some(character) = decoded {
                let mut utf8 = [0; 4];
                sink(character.encode_utf8(&mut utf8).as_bytes());
            }
        }
        if pending.is_empty() {
            sink(&chunk[run..]);
        }
        Ok(())
    }

    fn finish(self) -> Result<(), JournalError> {
        if !self.pending.is_empty() || self.high.is_some() {
            return Err(JournalError::Corrupt);
        }
        Ok(())
    }
}

pub(crate) struct Unescaped<'a> {
    file: &'a File,
    offset: u64,
    len: u64,
    raw: Vec<u8>,
    out: Vec<u8>,
    served: usize,
    state: Option<Unescaper>,
}

impl<'a> Unescaped<'a> {
    pub(crate) fn new(file: &'a File, offset: u64, len: u64, chunk: usize) -> Self {
        Self {
            file,
            offset,
            len,
            raw: vec![0; chunk],
            out: Vec::with_capacity(chunk),
            served: 0,
            state: Some(Unescaper::default()),
        }
    }
}

impl BufRead for Unescaped<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        let invalid = |_corrupt: JournalError| io::Error::from(io::ErrorKind::InvalidData);
        while self.served == self.out.len() {
            let Some(state) = self.state.as_mut() else {
                return Ok(&[]);
            };
            if self.len == 0 {
                self.state
                    .take()
                    .map_or(Ok(()), Unescaper::finish)
                    .map_err(invalid)?;
                return Ok(&[]);
            }
            let count =
                usize::try_from(self.len.min(self.raw.len() as u64)).unwrap_or(self.raw.len());
            self.file
                .read_exact_at(&mut self.raw[..count], self.offset)?;
            self.offset += count as u64;
            self.len -= count as u64;
            self.out.clear();
            self.served = 0;
            let out = &mut self.out;
            state
                .feed(&self.raw[..count], &mut |bytes| {
                    out.extend_from_slice(bytes)
                })
                .map_err(invalid)?;
        }
        Ok(&self.out[self.served..])
    }

    fn consume(&mut self, amount: usize) {
        self.served = (self.served + amount).min(self.out.len());
    }
}

impl Read for Unescaped<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let count = buf.len().min(available.len());
        buf[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
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
                        std::str::from_utf8(&atom[3..7])
                            .map_err(|_invalid| JournalError::Corrupt)?,
                        16,
                    )
                    .map_err(|_invalid| JournalError::Corrupt)?;
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
pub(crate) mod tests;

#[derive(Default)]
pub(crate) struct Boot {
    pub reset: u32,
    pub requeued: u32,
    pub crashed_files: u32,
    pub overflowed: u32,
    pub tasks: Vec<crate::memory::engine::SlotTask>,
}

impl Journal {
    #[cfg(test)]
    pub(crate) fn open_records(
        dir: &std::path::Path,
        max_age: std::time::Duration,
        retention: std::time::Duration,
        seal_retention: std::time::Duration,
        queue: NonZeroUsize,
    ) -> Result<(Self, Boot), JournalError> {
        let journal = Self::create(dir, max_age, retention, seal_retention)?;
        let boot = journal.load_summary_tasks(queue)?;
        Ok((journal, boot))
    }

    pub(crate) fn load_summary_tasks(&self, queue: NonZeroUsize) -> Result<Boot, JournalError> {
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
                        boot.reset = boot.reset.saturating_add(1);
                    }
                    boot.crashed_files = boot.crashed_files.saturating_add(1);
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
                            boot_record(&mut append, &stem, &head, queue, &mut boot)?;
                        }
                    }
                    Ok(None) => {}
                    Err(_unloadable) => {
                        fs::remove_file(&path)?;
                        boot.reset = boot.reset.saturating_add(1);
                    }
                }
                fs::remove_file(&marker)?;
            }
        }
        Ok(boot)
    }
}

fn boot_record(
    append: &mut File,
    stem: &str,
    head: &Head,
    queue: NonZeroUsize,
    boot: &mut Boot,
) -> Result<(), JournalError> {
    let mut lowest = None;
    let at_ms = super::millis(std::time::SystemTime::now());
    for (index, slot) in head.header.summaries.iter().enumerate() {
        let part =
            NonZeroU8::new(u8::try_from(index + 1).map_err(|_invalid| JournalError::Corrupt)?)
                .ok_or(JournalError::Corrupt)?;
        match slot {
            Slot::Running => {
                append_boot_slot(
                    append,
                    SlotLine {
                        at_ms,
                        record: head.header.id,
                        part,
                        slot: Slot::Queued,
                        subject: Subject::Record,
                    },
                )?;
                boot.requeued = boot.requeued.saturating_add(1);
                lowest.get_or_insert(part);
            }
            Slot::Queued => {
                lowest.get_or_insert(part);
            }
            Slot::Generated(_) | Slot::Error(_) => {}
        }
    }
    if let Some(part) = lowest {
        if boot.tasks.len() < queue.get() {
            boot.tasks.push(crate::memory::engine::SlotTask {
                stem: stem.to_owned(),
                record: head.header.id,
                part,
            });
        } else {
            for index in usize::from(part.get() - 1)..head.header.summaries.len() {
                if !head.header.summaries[index].is_pending() {
                    continue;
                }
                let part = NonZeroU8::new(
                    u8::try_from(index + 1).map_err(|_invalid| JournalError::Corrupt)?,
                )
                .ok_or(JournalError::Corrupt)?;
                append_boot_slot(
                    append,
                    SlotLine {
                        at_ms,
                        record: head.header.id,
                        part,
                        slot: Slot::Error(SummaryError {
                            kind: SummaryFailure::Overflow,
                            detail: "summary queue is full".to_owned(),
                        }),
                        subject: Subject::Record,
                    },
                )?;
                boot.overflowed = boot.overflowed.saturating_add(1);
            }
        }
    }
    Ok(())
}

fn append_boot_slot(file: &mut File, line: SlotLine) -> Result<(), JournalError> {
    let mut bytes = serde_json::to_vec(&SummaryLine { s: line }).map_err(corrupt)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    Ok(())
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
