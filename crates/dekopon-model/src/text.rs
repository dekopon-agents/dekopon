use std::{fmt, fs::File, io, os::unix::fs::FileExt, sync::Arc};

use bytes::Bytes;

use crate::body::{Chunk, OpaqueSource};

#[derive(Clone)]
pub enum Text {
    Owned(String),
    File {
        file: Arc<File>,
        offset: u64,
        len: u64,
    },
    Opaque(OpaqueSource),
}

impl fmt::Debug for Text {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Owned(text) => formatter.debug_tuple("Owned").field(text).finish(),
            Self::File { offset, len, .. } => formatter
                .debug_struct("File")
                .field("offset", offset)
                .field("len", len)
                .finish_non_exhaustive(),
            Self::Opaque(source) => formatter
                .debug_struct("Opaque")
                .field("bytes", &source.len())
                .finish_non_exhaustive(),
        }
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Self::Owned(text)
    }
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self::Owned(text.to_owned())
    }
}

impl Text {
    pub fn as_owned(&self) -> Option<&str> {
        match self {
            Self::Owned(text) => Some(text),
            Self::File { .. } | Self::Opaque(_) => None,
        }
    }

    pub fn chunks(&self, chunks: &mut Vec<Chunk>) -> Result<(), serde_json::Error> {
        match self {
            Self::Owned(text) => chunks.push(Chunk::Literal(crate::body::scalar_bytes(text)?)),
            Self::File { file, offset, len } => {
                chunks.push(Chunk::Literal(Bytes::from_static(b"\"")));
                chunks.push(Chunk::File {
                    file: Arc::clone(file),
                    offset: *offset,
                    len: *len,
                });
                chunks.push(Chunk::Literal(Bytes::from_static(b"\"")));
            }
            Self::Opaque(source) => chunks.push(Chunk::Opaque(source.clone())),
        }
        Ok(())
    }

    pub fn prefix(&self, limit: u64) -> io::Result<Self> {
        match self {
            Self::Owned(text) => {
                let end = text.floor_char_boundary(
                    usize::try_from(limit).unwrap_or(usize::MAX).min(text.len()),
                );
                Ok(Self::Owned(text[..end].to_owned()))
            }
            Self::File { file, offset, len } => Ok(Self::File {
                file: Arc::clone(file),
                offset: *offset,
                len: safe_prefix(file, *offset, *len, limit)?,
            }),
            Self::Opaque(source) => Ok(Self::Opaque(source.clone())),
        }
    }
}

fn safe_prefix(file: &File, offset: u64, len: u64, limit: u64) -> io::Result<u64> {
    let mut end = len.min(limit);
    if end == len || end == 0 {
        return Ok(end);
    }
    let start = end.saturating_sub(6);
    let mut tail = [0u8; 7];
    let count = usize::try_from(end - start + 1).map_err(io::Error::other)?;
    let position = offset.checked_add(start).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "journal slice offset overflow")
    })?;
    file.read_exact_at(&mut tail[..count], position)?;
    while end > start && tail[(end - start) as usize] & 0xc0 == 0x80 {
        end -= 1;
    }
    let candidate = end;
    for index in (start..end).rev() {
        if tail[(index - start) as usize] != b'\\' {
            continue;
        }
        let mut slash = index;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let from = slash.saturating_sub(buffer.len() as u64);
            let count = (slash - from) as usize;
            if count == 0 {
                break;
            }
            file.read_exact_at(
                &mut buffer[..count],
                offset.checked_add(from).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "journal slice offset overflow")
                })?,
            )?;
            let run = buffer[..count]
                .iter()
                .rev()
                .take_while(|byte| **byte == b'\\')
                .count();
            slash -= run as u64;
            if run < count {
                break;
            }
        }
        let run = index - slash + 1;
        if run % 2 == 0 {
            continue;
        }
        let escape = tail.get((index - start + 1) as usize).copied();
        let width = if escape == Some(b'u') { 6 } else { 2 };
        if index + width > candidate {
            return Ok(index);
        }
        break;
    }
    Ok(end)
}

impl PartialEq for Text {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Owned(left), Self::Owned(right)) => left == right,
            (
                Self::File {
                    file: left,
                    offset: a,
                    len: b,
                },
                Self::File {
                    file: right,
                    offset: c,
                    len: d,
                },
            ) => Arc::ptr_eq(left, right) && a == c && b == d,
            (Self::Opaque(left), Self::Opaque(right)) => match (left, right) {
                (OpaqueSource::Owned(left), OpaqueSource::Owned(right)) => left == right,
                (
                    OpaqueSource::File {
                        file: left,
                        offset: a,
                        len: b,
                    },
                    OpaqueSource::File {
                        file: right,
                        offset: c,
                        len: d,
                    },
                ) => Arc::ptr_eq(left, right) && a == c && b == d,
                (OpaqueSource::Owned(_), OpaqueSource::File { .. })
                | (OpaqueSource::File { .. }, OpaqueSource::Owned(_)) => false,
            },
            (Self::Owned(_), Self::File { .. } | Self::Opaque(_))
            | (Self::File { .. }, Self::Owned(_) | Self::Opaque(_))
            | (Self::Opaque(_), Self::Owned(_) | Self::File { .. }) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Text;
    use std::{io::Write, sync::Arc};

    #[test]
    fn cuts_never_end_inside_an_escape_or_utf8_character() {
        let source = "a\né\\\"\u{0001}z";
        let encoded = serde_json::to_string(source).unwrap();
        let escaped = &encoded.as_bytes()[1..encoded.len() - 1];
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"prefix").unwrap();
        file.write_all(escaped).unwrap();
        let file = Arc::new(file);
        let text = Text::File {
            file,
            offset: 6,
            len: escaped.len() as u64,
        };
        for limit in 0..=escaped.len() {
            let cut = text.prefix(limit as u64).unwrap();
            let Text::File { len, .. } = cut else {
                panic!("file slice")
            };
            assert!(len <= limit as u64);
            let mut value = vec![b'\"'];
            value.extend_from_slice(&escaped[..len as usize]);
            value.push(b'\"');
            let decoded: String = serde_json::from_slice(&value).unwrap();
            assert!(source.starts_with(&decoded));
        }
    }
}
