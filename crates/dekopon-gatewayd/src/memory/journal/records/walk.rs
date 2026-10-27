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

pub(super) fn window(file: File, limit: NonZeroUsize) -> Result<Snapshot, JournalError> {
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
    let mut extent = lines.end;
    let mut records = Vec::new();
    let mut header_ends = Vec::new();
    let mut above = false;
    while records.len() < limit.get() {
        let Some((at, len)) = lines.previous()? else {
            break;
        };
        let mut tag = [0; 5];
        if len < tag.len() as u64 {
            return Err(JournalError::Corrupt);
        }
        file.read_exact_at(&mut tag, at)?;
        match &tag {
            b"{\"b\":" if !above => {
                extent = at;
                continue;
            }
            b"{\"s\":" | b"{\"p\":" | b"{\"c\":" => {
                above = true;
                continue;
            }
            b"{\"h\":" => {}
            _ => return Err(JournalError::Corrupt),
        }
        above = true;
        let mut raw = vec![0; usize::try_from(len).map_err(|_invalid| JournalError::Corrupt)?];
        file.read_exact_at(&mut raw, at)?;
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
        header_ends.push(at + len);
        records.push(Head {
            header,
            body_at,
            asset_lines: Vec::new(),
            raw_asset_lines: Vec::new(),
            arrived_raw,
        });
    }
    records.reverse();
    header_ends.reverse();
    let mut cursor = lines.end;
    let mut line = Vec::new();
    for index in 0..=records.len() {
        let until = records.get(index).map_or(extent, |head| head.body_at);
        let mut region = io::BufReader::new(SliceReader {
            file: &file,
            offset: cursor,
            left: until.saturating_sub(cursor),
        });
        loop {
            line.clear();
            if region.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            let side = Side::decode(&line)?;
            if let Some(head) = records
                .binary_search_by_key(&side.record(), |head| head.header.id)
                .ok()
                .and_then(|found| records.get_mut(found))
            {
                side.apply(head, &line)?;
            } else if records
                .first()
                .is_none_or(|head| side.record() >= head.header.id)
            {
                return Err(JournalError::Corrupt);
            }
        }
        if let Some(end) = header_ends.get(index) {
            cursor = *end;
        }
    }
    Ok(Snapshot { file, records })
}
