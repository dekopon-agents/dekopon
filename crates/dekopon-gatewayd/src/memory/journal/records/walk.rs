use super::*;

struct ReverseLines<'a> {
    file: &'a File,
    end: u64,
    buffer: [u8; BUFFER],
}

impl ReverseLines<'_> {
    fn previous(&mut self) -> Result<Option<(u64, u64)>, JournalError> {
        if self.end == 0 {
            return Ok(None);
        }
        let end = self.end;
        let mut cursor = end - 1;
        loop {
            let from = cursor.saturating_sub(BUFFER as u64);
            let count = (cursor - from) as usize;
            self.file.read_exact_at(&mut self.buffer[..count], from)?;
            if let Some(index) = self.buffer[..count].iter().rposition(|byte| *byte == b'\n') {
                self.end = from + index as u64 + 1;
                return Ok(Some((self.end, end - self.end)));
            }
            if from == 0 {
                self.end = 0;
                return Ok(Some((0, end)));
            }
            cursor = from;
        }
    }
}

pub(crate) fn window(file: File, limit: NonZeroUsize) -> Result<Snapshot, JournalError> {
    let file = Arc::new(file);
    let mut lines = ReverseLines {
        file: &file,
        end: file.metadata()?.len(),
        buffer: [0; BUFFER],
    };
    if lines.end > 0 {
        let mut byte = [0];
        file.read_exact_at(&mut byte, lines.end - 1)?;
        if byte[0] != b'\n' {
            let _torn = lines.previous()?;
        }
    }
    let mut records = Vec::new();
    let mut sides = Vec::new();
    while records.len() < limit.get() {
        let Some((at, len)) = lines.previous()? else {
            break;
        };
        let mut tag = [0; 5];
        if len < tag.len() as u64 {
            return Err(JournalError::Corrupt);
        }
        file.read_exact_at(&mut tag, at)?;
        if &tag == b"{\"b\":" {
            continue;
        }
        if !matches!(&tag, b"{\"h\":" | b"{\"s\":" | b"{\"p\":" | b"{\"c\":") {
            return Err(JournalError::Corrupt);
        }
        let mut raw = vec![0; usize::try_from(len).map_err(|_invalid| JournalError::Corrupt)?];
        file.read_exact_at(&mut raw, at)?;
        if &tag == b"{\"h\":" {
            let header = decode_line::<HeaderLine>(&raw)?.h;
            validate_header(&header, header.body_len)?;
            if records
                .last()
                .is_some_and(|newer: &Head| header.next_record_id != newer.header.id.0)
            {
                return Err(JournalError::Corrupt);
            }
            let body_at = at
                .checked_sub(header.body_len)
                .ok_or(JournalError::Corrupt)?;
            file.read_exact_at(&mut tag, body_at)?;
            if &tag != b"{\"b\":" {
                return Err(JournalError::Corrupt);
            }
            let arrived_raw = decode_line::<RawHeaderLine>(&raw)?.h.arrived;
            lines.end = body_at;
            records.push(Head {
                header,
                body_at,
                asset_lines: Vec::new(),
                raw_asset_lines: Vec::new(),
                arrived_raw,
            });
        } else {
            sides.push((Side::decode(&raw)?, raw));
        }
    }
    records.reverse();
    for (side, raw) in sides.into_iter().rev() {
        if let Some(head) = records
            .iter_mut()
            .find(|head| head.header.id == side.record())
        {
            side.apply(head, &raw)?;
        } else if records
            .first()
            .is_none_or(|head| side.record() >= head.header.id)
        {
            return Err(JournalError::Corrupt);
        }
    }
    Ok(Snapshot { file, records })
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Tail {
    Clean,
    Torn,
}

pub(crate) fn tail(file: &File) -> Result<Tail, JournalError> {
    let end = file.metadata()?.len();
    if end == 0 {
        return Ok(Tail::Clean);
    }
    let mut byte = [0];
    file.read_exact_at(&mut byte, end - 1)?;
    if byte[0] != b'\n' {
        return Ok(Tail::Torn);
    }
    let mut lines = ReverseLines {
        file,
        end,
        buffer: [0; BUFFER],
    };
    let Some((at, len)) = lines.previous()? else {
        return Ok(Tail::Clean);
    };
    let mut tag = [0; 5];
    if len < tag.len() as u64 {
        return Ok(Tail::Torn);
    }
    file.read_exact_at(&mut tag, at)?;
    Ok(if &tag == b"{\"b\":" {
        Tail::Torn
    } else {
        Tail::Clean
    })
}
