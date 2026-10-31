use std::{io, num::NonZeroU64, time::SystemTime};

use dekopon_agent::memory_tools::{AttachmentTargets, MemoryRefusal, MemoryTools, Rung, Target};
use parking_lot::Mutex;

use super::{
    Journal, JournalError, millis,
    records::{Head, admits, record_text},
};
use crate::asset::MemoryAssets;
use crate::memory::record::{GrantDigest, PinLine, RecordId, Slot, Subject};

const MAX_READ_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct Admission {
    pub grant: GrantDigest,
    pub horizon_ms: u64,
    pub sealed_at_ms: Option<u64>,
}

/// Reads walk the stem on their own descriptor without the journal lock; pins wait here until
/// `persist` appends them after delivery.
pub(crate) struct JournalMemoryTools<'a> {
    journal: &'a Journal,
    stem: String,
    admission: Admission,
    queued: Mutex<Vec<PinLine>>,
    assets: Option<MemoryAssets<'a>>,
}

impl<'a> JournalMemoryTools<'a> {
    pub(crate) fn new(journal: &'a Journal, stem: String, admission: Admission) -> Self {
        Self {
            journal,
            stem,
            admission,
            queued: Mutex::new(Vec::new()),
            assets: None,
        }
    }

    pub(crate) fn with_assets(mut self, assets: MemoryAssets<'a>) -> Self {
        self.assets = Some(assets);
        self
    }

    pub(crate) fn into_pins(self) -> Vec<PinLine> {
        self.queued.into_inner()
    }

    fn lookup(&self, target: Target) -> Result<Located, MemoryRefusal> {
        let snapshot = match target {
            Target::Record(record) => {
                let record = NonZeroU64::new(record)
                    .map(RecordId)
                    .ok_or(MemoryRefusal::RecordUnknown)?;
                self.journal.record_lookup(&self.stem, record)
            }
            Target::Asset(_) => {
                if self.assets.is_none() {
                    return Err(MemoryRefusal::RepresentationUnavailable {
                        available: Vec::new(),
                    });
                }
                self.journal.record_snapshot(&self.stem, false)
            }
        }
        .map_err(|error| match error {
            JournalError::Io {
                kind: io::ErrorKind::NotFound,
            } => MemoryRefusal::RecordUnknown,
            JournalError::Io { kind } => MemoryRefusal::Storage { kind },
            JournalError::Corrupt => MemoryRefusal::Storage {
                kind: io::ErrorKind::InvalidData,
            },
        })?
        .ok_or(MemoryRefusal::RecordUnknown)?;
        let file = snapshot.file;
        let head = snapshot
            .records
            .into_iter()
            .find(|head| match target {
                Target::Record(id) => head.header.id.0.get() == id,
                Target::Asset(number) => {
                    self.admitted(head)
                        && head
                            .header
                            .arrived
                            .iter()
                            .any(|asset| asset.number == number)
                }
            })
            .ok_or(MemoryRefusal::RecordUnknown)?;
        if !self.admitted(&head) {
            return Err(MemoryRefusal::RecordIneligible);
        }
        let asset_summaries = match target {
            Target::Record(_) => None,
            Target::Asset(number) => head
                .header
                .arrived
                .iter()
                .find(|asset| asset.number == number)
                .map(|asset| asset.join(&head.asset_lines).summaries),
        };
        Ok(Located {
            head,
            file,
            subject: match target {
                Target::Record(_) => Subject::Record,
                Target::Asset(number) => Subject::Asset(number),
            },
            asset_summaries,
        })
    }

    fn admitted(&self, head: &Head) -> bool {
        admits(
            &head.header,
            &self.admission.grant,
            self.admission.horizon_ms,
            self.admission.sealed_at_ms,
        )
    }

    fn pinned(&self, record: RecordId, subject: Subject) -> Option<Option<Rung>> {
        self.queued
            .lock()
            .iter()
            .rev()
            .find(|line| line.record == record && line.subject == subject)
            .map(|line| line.pin)
    }
}

pub(crate) fn available(head: &Head) -> Vec<Rung> {
    let mut rungs = vec![Rung::Native];
    rungs.extend(
        head.header
            .summaries
            .iter()
            .enumerate()
            .filter(|(_, slot)| matches!(slot, Slot::Generated(_)))
            .filter_map(|(index, _)| {
                u8::try_from(index + 1)
                    .ok()
                    .and_then(std::num::NonZeroU8::new)
                    .map(Rung::Summary)
            }),
    );
    rungs.push(Rung::Metadata);
    rungs
}

pub(crate) fn envelope(head: &Head, rung: Rung) -> String {
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_millis(head.header.at_ms);
    let time = humantime::format_rfc3339_seconds(at);
    let detail = match rung {
        Rung::Summary(part) => format!("summary {part}/{}", head.header.summaries.len()),
        Rung::Native | Rung::Metadata => {
            let items = head
                .header
                .spans
                .iter()
                .map(|span| span.item)
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            format!("{items} items · {} B", head.header.body_len)
        }
    };
    format!("[memory #{} · {time} · {detail}]", head.header.id.0)
}

fn window(total: u64, range: Option<(u64, u64)>) -> Result<(u64, u64), MemoryRefusal> {
    match range {
        None if total <= MAX_READ_BYTES => Ok((0, total)),
        Some((start, end)) if start < end && end - start <= MAX_READ_BYTES && end <= total => {
            Ok((start, end))
        }
        None | Some(_) => Err(MemoryRefusal::Range { len: total }),
    }
}

fn utf8(bytes: Vec<u8>, total: u64) -> Result<String, MemoryRefusal> {
    String::from_utf8(bytes).map_err(|_cut| MemoryRefusal::Range { len: total })
}

fn storage(error: JournalError) -> MemoryRefusal {
    match error {
        JournalError::Io { kind } => MemoryRefusal::Storage { kind },
        JournalError::Corrupt => MemoryRefusal::Storage {
            kind: io::ErrorKind::InvalidData,
        },
    }
}

struct Located {
    head: Head,
    file: std::sync::Arc<std::fs::File>,
    subject: Subject,
    asset_summaries: Option<Vec<Slot>>,
}

impl Located {
    fn summaries(&self) -> &[Slot] {
        self.asset_summaries
            .as_deref()
            .unwrap_or(&self.head.header.summaries)
    }

    fn asset(&self) -> Option<&crate::memory::record::ArrivedAsset> {
        match self.subject {
            Subject::Record => None,
            Subject::Asset(number) => self
                .head
                .header
                .arrived
                .iter()
                .find(|asset| asset.number == number),
        }
    }
}

enum Purpose {
    Read,
    Pin,
}

enum Representation<'a> {
    Native,
    Summary {
        part: std::num::NonZeroU8,
        text: &'a str,
    },
    Metadata,
}

impl Representation<'_> {
    fn rung(&self) -> Rung {
        match self {
            Self::Native => Rung::Native,
            Self::Summary { part, .. } => Rung::Summary(*part),
            Self::Metadata => Rung::Metadata,
        }
    }
}

impl JournalMemoryTools<'_> {
    fn rung<'a>(
        &self,
        located: &'a Located,
        id: &str,
        purpose: Purpose,
    ) -> Result<Representation<'a>, MemoryRefusal> {
        let asset = located.asset();
        let native = match (asset, purpose) {
            (None, Purpose::Read | Purpose::Pin) => true,
            (Some(_), Purpose::Read) => false,
            (Some(asset), Purpose::Pin) => self
                .assets
                .is_some_and(|assets| assets.native_eligible(asset.number, asset.kind)),
        };
        let available = match asset {
            Some(_) => asset_available(located.summaries(), native),
            None => available(&located.head),
        };
        let rung: Rung = id
            .parse()
            .map_err(|_unknown| MemoryRefusal::UnknownRepresentation {
                available: available.clone(),
            })?;
        match rung {
            Rung::Metadata => Ok(Representation::Metadata),
            Rung::Native if native => Ok(Representation::Native),
            Rung::Native => Err(MemoryRefusal::RepresentationUnavailable { available }),
            Rung::Summary(part) => match located.summaries().get(usize::from(part.get() - 1)) {
                Some(Slot::Generated(text)) => Ok(Representation::Summary { part, text }),
                Some(Slot::Queued | Slot::Running | Slot::Error(_)) => {
                    Err(MemoryRefusal::RepresentationUnavailable { available })
                }
                None => Err(MemoryRefusal::UnknownRepresentation { available }),
            },
        }
    }
}

pub(crate) fn asset_available(slots: &[Slot], native: bool) -> Vec<Rung> {
    let mut rungs = Vec::new();
    if native {
        rungs.push(Rung::Native);
    }
    rungs.extend(slots.iter().enumerate().filter_map(|(index, slot)| {
        match slot {
            Slot::Generated(_) => u8::try_from(index + 1)
                .ok()
                .and_then(std::num::NonZeroU8::new)
                .map(Rung::Summary),
            Slot::Queued | Slot::Running | Slot::Error(_) => None,
        }
    }));
    rungs.push(Rung::Metadata);
    rungs
}

fn asset_metadata(head: &Head, asset: &crate::memory::record::ArrivedAsset) -> String {
    let kind = asset.kind.label();
    let bytes = asset
        .bytes
        .map_or_else(|| "size unknown".to_owned(), |bytes| format!("{bytes} B"));
    format!(
        "[chat-asset:{} · {kind} · {bytes} · arrived with memory #{}]",
        asset.number.get(),
        head.header.id.0
    )
}

impl MemoryTools for JournalMemoryTools<'_> {
    fn attachment_targets(&self) -> AttachmentTargets {
        match self.assets {
            Some(_) => AttachmentTargets::On,
            None => AttachmentTargets::Off,
        }
    }
    fn read(
        &self,
        target: Target,
        representation_id: &str,
        range: Option<(u64, u64)>,
    ) -> Result<String, MemoryRefusal> {
        let located = self.lookup(target)?;
        let representation = self.rung(&located, representation_id, Purpose::Read)?;
        let text = match representation {
            Representation::Native => {
                let total = record_text(&located.file, &located.head, None)
                    .map_err(storage)?
                    .total;
                let (start, end) = window(total, range)?;
                return utf8(
                    record_text(&located.file, &located.head, Some((start, end)))
                        .map_err(storage)?
                        .selected,
                    total,
                );
            }
            Representation::Summary { text, .. } => text.to_owned(),
            Representation::Metadata => match located.asset() {
                Some(asset) => asset_metadata(&located.head, asset),
                None => envelope(&located.head, Rung::Metadata),
            },
        };
        let total = text.len() as u64;
        let (start, end) = window(total, range)?;
        text.get(start as usize..end as usize)
            .map(str::to_owned)
            .ok_or(MemoryRefusal::Range { len: total })
    }

    fn set_pin(&self, target: Target, representation_id: &str) -> Result<String, MemoryRefusal> {
        let located = self.lookup(target)?;
        let rung = self.rung(&located, representation_id, Purpose::Pin)?.rung();
        self.queued.lock().push(PinLine {
            at_ms: millis(SystemTime::now()),
            record: located.head.header.id,
            subject: located.subject,
            pin: Some(rung),
        });
        Ok(match target {
            Target::Record(id) => format!("pinned record {id} to {rung}"),
            Target::Asset(number) => format!("pinned asset {} to {rung}", number.get()),
        })
    }

    fn clear_pin(&self, target: Option<Target>) -> Result<String, MemoryRefusal> {
        let now = millis(SystemTime::now());
        let mut pins = Vec::new();
        if let Some(target) = target {
            let located = self.lookup(target)?;
            pins.push(PinLine {
                at_ms: now,
                record: located.head.header.id,
                subject: located.subject,
                pin: None,
            });
        } else {
            let Some(snapshot) = self
                .journal
                .record_snapshot(&self.stem, false)
                .map_err(storage)?
            else {
                return Ok("no eligible pin found".into());
            };
            for head in snapshot.records.iter().filter(|head| self.admitted(head)) {
                if self
                    .pinned(head.header.id, Subject::Record)
                    .unwrap_or(head.header.pin)
                    .is_some()
                {
                    pins.push(PinLine {
                        at_ms: now,
                        record: head.header.id,
                        subject: Subject::Record,
                        pin: None,
                    });
                }
                for asset in head.header.arrived.iter().filter(|_| self.assets.is_some()) {
                    let subject = Subject::Asset(asset.number);
                    if self
                        .pinned(head.header.id, subject)
                        .unwrap_or(asset.join(&head.asset_lines).pin)
                        .is_some()
                    {
                        pins.push(PinLine {
                            at_ms: now,
                            record: head.header.id,
                            subject,
                            pin: None,
                        });
                    }
                }
            }
        }
        if pins.is_empty() {
            return Ok("no eligible pin found".into());
        }
        self.queued.lock().extend(pins);
        Ok("pin cleared".into())
    }
}
