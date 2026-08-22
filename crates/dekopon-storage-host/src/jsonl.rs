//! Curated JSONL operations over the invocation overlay.

use dekopon_capability::StorageInterface;

use crate::{StorageHostError, StorageTransaction};

/// One bounded JSONL read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsonlChunk {
    pub bytes: Vec<u8>,
    pub next_offset: u64,
    pub eof: bool,
}

impl StorageTransaction {
    pub fn jsonl_size(&mut self, name: &str) -> Result<u64, StorageHostError> {
        self.require_jsonl()?;
        self.note_call()?;
        let token = self.ensure_entry(name)?;
        self.entries[&token]
            .size()
            .ok_or(StorageHostError::NotFound)
    }

    pub fn jsonl_read_chunk(
        &mut self,
        name: &str,
        offset: u64,
        max_bytes: u32,
    ) -> Result<JsonlChunk, StorageHostError> {
        self.require_jsonl()?;
        self.note_call()?;
        if max_bytes == 0 {
            return Err(StorageHostError::InvalidArgument);
        }
        self.charge_read(u64::from(max_bytes))?;
        let token = self.ensure_entry(name)?;
        let size = self.entries[&token]
            .size()
            .ok_or(StorageHostError::NotFound)?;
        if offset > size {
            return Err(StorageHostError::InvalidArgument);
        }
        let maximum = usize::try_from(u64::from(max_bytes).min(size - offset))
            .map_err(|_| StorageHostError::Arithmetic)?;
        let bytes = if self.entries[&token].loaded {
            let data = self.entries[&token]
                .data
                .as_ref()
                .ok_or(StorageHostError::NotFound)?;
            let start = usize::try_from(offset).map_err(|_| StorageHostError::InvalidArgument)?;
            data[start..start + maximum].to_vec()
        } else {
            self.namespace
                .data_directory
                .read_at(&token, offset, maximum)?
        };
        let next_offset = offset
            .checked_add(bytes.len() as u64)
            .ok_or(StorageHostError::Arithmetic)?;
        Ok(JsonlChunk {
            bytes,
            next_offset,
            eof: next_offset == size,
        })
    }

    pub fn jsonl_append(
        &mut self,
        name: &str,
        expected_size: u64,
        record: &[u8],
    ) -> Result<u64, StorageHostError> {
        self.require_jsonl()?;
        self.note_call()?;
        if record.contains(&b'\n')
            || record.contains(&b'\r')
            || serde_json::from_slice::<serde_json::Value>(record).is_err()
        {
            return Err(StorageHostError::InvalidArgument);
        }
        let write = u64::try_from(record.len())
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(StorageHostError::Arithmetic)?;
        self.charge_write(write)?;
        let token = self.ensure_loaded(name)?;
        let current_size = self.entries[&token].data.as_ref().map_or(0, Vec::len);
        if current_size as u64 != expected_size {
            return Err(StorageHostError::Busy);
        }
        let was_present = self.entries[&token].data.is_some();
        let mut replacement = self
            .entries
            .get_mut(&token)
            .expect("loaded entry")
            .data
            .take()
            .unwrap_or_default();
        replacement.reserve(record.len().saturating_add(1));
        replacement.extend_from_slice(record);
        replacement.push(b'\n');
        if let Err(error) = self.reserve_candidate(&[(&token, Some(replacement.as_slice()))]) {
            replacement.truncate(current_size);
            self.entries.get_mut(&token).expect("loaded entry").data =
                was_present.then_some(replacement);
            return Err(error);
        }
        let entry = self.entries.get_mut(&token).expect("loaded entry");
        entry.data = Some(replacement);
        entry.dirty = true;
        Ok(entry.data.as_ref().map_or(0, |bytes| bytes.len() as u64))
    }

    pub fn jsonl_replace(
        &mut self,
        name: &str,
        expected_size: u64,
        contents: &[u8],
    ) -> Result<(), StorageHostError> {
        self.require_jsonl()?;
        self.note_call()?;
        if !valid_jsonl(contents) {
            return Err(StorageHostError::InvalidArgument);
        }
        self.charge_write(contents.len() as u64)?;
        let token = self.ensure_loaded(name)?;
        let current_size = self.entries[&token]
            .data
            .as_ref()
            .map_or(0, |bytes| bytes.len() as u64);
        if current_size != expected_size {
            return Err(StorageHostError::Busy);
        }
        self.reserve_candidate(&[(&token, Some(contents))])?;
        let entry = self.entries.get_mut(&token).expect("loaded entry");
        entry.data = Some(contents.to_vec());
        entry.dirty = true;
        Ok(())
    }

    fn require_jsonl(&self) -> Result<(), StorageHostError> {
        if self.interface == StorageInterface::Jsonl {
            Ok(())
        } else {
            Err(StorageHostError::PermissionDenied)
        }
    }
}

fn valid_jsonl(contents: &[u8]) -> bool {
    if contents.is_empty() {
        return true;
    }
    if !contents.ends_with(b"\n") {
        return false;
    }
    contents[..contents.len() - 1]
        .split(|byte| *byte == b'\n')
        .all(|line| !line.is_empty() && serde_json::from_slice::<serde_json::Value>(line).is_ok())
}

#[cfg(test)]
mod tests {
    use super::valid_jsonl;

    #[test]
    fn only_complete_jsonl_is_replaceable() {
        assert!(valid_jsonl(b""));
        assert!(valid_jsonl(b"{\"a\":1}\n{\"a\":2}\n"));
        assert!(!valid_jsonl(b"{\"a\":1}"));
        assert!(!valid_jsonl(b"not-json\n"));
    }
}
