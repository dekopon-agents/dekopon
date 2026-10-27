use std::{io, num::NonZeroU64, time::SystemTime};

use dekopon_agent::memory_tools::{MemoryRefusal, MemoryTools, Rung};
use parking_lot::Mutex;

use super::{
    Journal, JournalError, millis,
    records::{Head, admits, record_text},
};
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
}

impl<'a> JournalMemoryTools<'a> {
    pub(crate) fn new(journal: &'a Journal, stem: String, admission: Admission) -> Self {
        Self {
            journal,
            stem,
            admission,
            queued: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn into_pins(self) -> Vec<PinLine> {
        self.queued.into_inner()
    }

    fn lookup(
        &self,
        record_id: u64,
    ) -> Result<(Head, std::sync::Arc<std::fs::File>), MemoryRefusal> {
        let id = NonZeroU64::new(record_id)
            .map(RecordId)
            .ok_or(MemoryRefusal::RecordUnknown)?;
        let snapshot = match self.journal.record_lookup(&self.stem, id) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None)
            | Err(JournalError::Io {
                kind: io::ErrorKind::NotFound,
            }) => return Err(MemoryRefusal::RecordUnknown),
            Err(JournalError::Io { kind }) => return Err(MemoryRefusal::Storage { kind }),
            Err(JournalError::Corrupt) => {
                return Err(MemoryRefusal::Storage {
                    kind: io::ErrorKind::InvalidData,
                });
            }
        };
        let file = snapshot.file;
        let head = snapshot
            .records
            .into_iter()
            .find(|head| head.header.id == id)
            .ok_or(MemoryRefusal::RecordUnknown)?;
        if !admits(
            &head.header,
            &self.admission.grant,
            self.admission.horizon_ms,
            self.admission.sealed_at_ms,
        ) {
            return Err(MemoryRefusal::RecordIneligible);
        }
        Ok((head, file))
    }

    fn rung(head: &Head, representation_id: &str) -> Result<Rung, MemoryRefusal> {
        let available = available(head);
        let rung: Rung =
            representation_id
                .parse()
                .map_err(|_unknown| MemoryRefusal::UnknownRepresentation {
                    available: available.clone(),
                })?;
        match rung {
            Rung::Native | Rung::Metadata => Ok(rung),
            Rung::Summary(part) => match head.header.summaries.get(usize::from(part.get() - 1)) {
                None => Err(MemoryRefusal::UnknownRepresentation { available }),
                Some(Slot::Generated(_)) => Ok(rung),
                Some(Slot::Queued | Slot::Running | Slot::Error(_)) => {
                    Err(MemoryRefusal::RepresentationUnavailable { available })
                }
            },
        }
    }

    fn pinned(&self, record: RecordId) -> Option<Option<Rung>> {
        self.queued
            .lock()
            .iter()
            .rev()
            .find(|line| line.record == record)
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

impl MemoryTools for JournalMemoryTools<'_> {
    fn read(
        &self,
        record_id: u64,
        representation_id: &str,
        range: Option<(u64, u64)>,
    ) -> Result<String, MemoryRefusal> {
        let (head, file) = self.lookup(record_id)?;
        let text = match Self::rung(&head, representation_id)? {
            Rung::Native => {
                let total = record_text(&file, &head, None).map_err(storage)?.total;
                let (start, end) = window(total, range)?;
                let selected = record_text(&file, &head, Some((start, end)))
                    .map_err(storage)?
                    .selected;
                return utf8(selected, total);
            }
            Rung::Summary(part) => match head.header.summaries.get(usize::from(part.get() - 1)) {
                Some(Slot::Generated(text)) => text.clone(),
                Some(Slot::Queued | Slot::Running | Slot::Error(_)) | None => {
                    return Err(MemoryRefusal::RepresentationUnavailable {
                        available: available(&head),
                    });
                }
            },
            Rung::Metadata => envelope(&head, Rung::Metadata),
        };
        let total = text.len() as u64;
        let (start, end) = window(total, range)?;
        text.get(start as usize..end as usize)
            .map(str::to_owned)
            .ok_or(MemoryRefusal::Range { len: total })
    }

    fn set_pin(&self, record_id: u64, representation_id: &str) -> Result<String, MemoryRefusal> {
        let (head, _file) = self.lookup(record_id)?;
        let rung = Self::rung(&head, representation_id)?;
        self.queued.lock().push(PinLine {
            at_ms: millis(SystemTime::now()),
            record: head.header.id,
            pin: Some(rung),
            subject: Subject::Record,
        });
        Ok(format!("pinned record {record_id} to {rung}"))
    }

    fn clear_pin(&self, record_id: Option<u64>) -> Result<String, MemoryRefusal> {
        let now = millis(SystemTime::now());
        let records: Vec<RecordId> = match record_id {
            Some(id) => vec![self.lookup(id)?.0.header.id],
            None => {
                let snapshot = match self.journal.record_snapshot(&self.stem, false) {
                    Ok(Some(snapshot)) => snapshot,
                    Ok(None) => return Ok("no eligible pin found".into()),
                    Err(JournalError::Io { kind }) => return Err(MemoryRefusal::Storage { kind }),
                    Err(JournalError::Corrupt) => {
                        return Err(MemoryRefusal::Storage {
                            kind: io::ErrorKind::InvalidData,
                        });
                    }
                };
                snapshot
                    .records
                    .iter()
                    .filter(|head| {
                        admits(
                            &head.header,
                            &self.admission.grant,
                            self.admission.horizon_ms,
                            self.admission.sealed_at_ms,
                        )
                    })
                    .filter(|head| {
                        self.pinned(head.header.id)
                            .unwrap_or(head.header.pin)
                            .is_some()
                    })
                    .map(|head| head.header.id)
                    .collect()
            }
        };
        if records.is_empty() {
            return Ok("no eligible pin found".into());
        }
        let mut queued = self.queued.lock();
        for record in records {
            queued.push(PinLine {
                at_ms: now,
                record,
                pin: None,
                subject: Subject::Record,
            });
        }
        Ok("pin cleared".into())
    }
}
