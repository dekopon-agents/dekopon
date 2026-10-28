use std::{io, os::unix::fs::FileExt as _};

use crate::{
    body::{Chunk, JsonBody, OpaqueSource},
    model::ModelMessage,
};

const SEGMENT_BYTES: usize = 16 * 1024;
// A cut moves back to a UTF-8 boundary by at most three bytes, so the grid leaves room for them.
const GRID: u64 = SEGMENT_BYTES as u64 - 3;

#[cfg(test)]
thread_local! {
    static READ_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub struct TranscriptSegments {
    chunks: Vec<Chunk>,
    len: u64,
}

impl ModelMessage {
    pub fn transcript_segments(messages: &[Self]) -> Result<TranscriptSegments, serde_json::Error> {
        let mut body = JsonBody::default();
        body.raw(b"[");
        for (index, message) in messages.iter().enumerate() {
            if index > 0 {
                body.raw(b",");
            }
            match message {
                Self::Journal { message } => message.transcript(&mut body)?,
                Self::System { .. }
                | Self::User { .. }
                | Self::Assistant { .. }
                | Self::ToolResults { .. } => body.value(&*message.prepared_for_trace())?,
            }
        }
        body.raw(b"]");
        let chunks = body.finish();
        let len = chunks.iter().map(chunk_len).sum();
        Ok(TranscriptSegments { chunks, len })
    }
}

impl TranscriptSegments {
    pub fn count(&self) -> u64 {
        self.len.div_ceil(GRID).max(1)
    }

    pub fn emit(&self, mut segment: impl FnMut(u64, &str)) -> io::Result<()> {
        let mut cutter = Cutter {
            buffer: Vec::with_capacity(SEGMENT_BYTES),
            start: 0,
            next_cut: GRID,
            len: self.len,
            index: 0,
        };
        for chunk in &self.chunks {
            match chunk {
                Chunk::Literal(bytes) => cutter.push(bytes, &mut segment),
                Chunk::File { file, offset, len } => {
                    let mut at = *offset;
                    let end = offset + len;
                    while at < end {
                        let room = cutter.room().min(end - at);
                        let from = cutter.buffer.len();
                        cutter.buffer.resize(from + room as usize, 0);
                        file.read_exact_at(&mut cutter.buffer[from..], at)?;
                        #[cfg(test)]
                        READ_BYTES.with(|read| read.set(read.get() + room));
                        at += room;
                        cutter.reached(&mut segment);
                    }
                }
                Chunk::Opaque(source) => {
                    cutter.push(opaque_marker(source).as_bytes(), &mut segment)
                }
            }
        }
        cutter.finish(&mut segment);
        Ok(())
    }
}

fn chunk_len(chunk: &Chunk) -> u64 {
    match chunk {
        Chunk::Literal(bytes) => bytes.len() as u64,
        Chunk::File { len, .. } => *len,
        Chunk::Opaque(source) => opaque_marker(source).len() as u64,
    }
}

fn opaque_marker(source: &OpaqueSource) -> String {
    format!("\"[opaque {} B]\"", source.len())
}

struct Cutter {
    buffer: Vec<u8>,
    start: u64,
    next_cut: u64,
    len: u64,
    index: u64,
}

impl Cutter {
    fn end(&self) -> u64 {
        self.start + self.buffer.len() as u64
    }

    fn room(&self) -> u64 {
        if self.next_cut < self.len {
            self.next_cut - self.end()
        } else {
            (SEGMENT_BYTES - self.buffer.len()) as u64
        }
    }

    fn push(&mut self, mut bytes: &[u8], segment: &mut impl FnMut(u64, &str)) {
        while !bytes.is_empty() {
            let take = usize::try_from(self.room())
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            self.reached(segment);
        }
    }

    fn reached(&mut self, segment: &mut impl FnMut(u64, &str)) {
        if self.next_cut < self.len && self.end() == self.next_cut {
            let cut = boundary(&self.buffer);
            self.index += 1;
            segment(self.index, &String::from_utf8_lossy(&self.buffer[..cut]));
            self.buffer.drain(..cut);
            self.start += cut as u64;
            self.next_cut += GRID;
        } else if self.buffer.len() == SEGMENT_BYTES {
            self.flush(segment);
        }
    }

    fn flush(&mut self, segment: &mut impl FnMut(u64, &str)) {
        self.index += 1;
        segment(self.index, &String::from_utf8_lossy(&self.buffer));
        self.start += self.buffer.len() as u64;
        self.buffer.clear();
    }

    fn finish(mut self, segment: &mut impl FnMut(u64, &str)) {
        if !self.buffer.is_empty() || self.index == 0 {
            self.flush(segment);
        }
    }
}

fn boundary(bytes: &[u8]) -> usize {
    let tail = bytes.len().saturating_sub(3);
    for at in (tail..bytes.len()).rev() {
        let byte = bytes[at];
        if byte & 0b1100_0000 != 0b1000_0000 {
            let width = match byte {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                _ => 4,
            };
            return if at + width > bytes.len() {
                at
            } else {
                bytes.len()
            };
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use std::{io::Write as _, sync::Arc};

    use super::{READ_BYTES, SEGMENT_BYTES};
    use crate::{
        body::OpaqueSource,
        model::{Backend, ModelKey, ModelMessage, ToolCallId},
        replay::{ReplayCall, ReplayMessage, ReplayReasoning},
        text::Text,
    };

    fn messages(mut text: impl FnMut(&str) -> Text, opaque: OpaqueSource) -> Vec<ModelMessage> {
        let long = "é".repeat(12 * 1024) + "\n\"tail\"";
        vec![
            ModelMessage::user("owned question"),
            ModelMessage::Journal {
                message: ReplayMessage::User(vec![text(&long), Text::Owned(" and more".into())]),
            },
            ModelMessage::Journal {
                message: ReplayMessage::Assistant {
                    content: Some(text("working")),
                    calls: vec![ReplayCall {
                        id: ToolCallId::from("call-1"),
                        name: "bash".into(),
                        arguments: text("{\"script\":\"ls\"}"),
                    }],
                    reasoning: vec![ReplayReasoning {
                        model: ModelKey::new(Backend::OpenRouter, "alias"),
                        value: opaque,
                    }],
                },
            },
            ModelMessage::Journal {
                message: ReplayMessage::Tool {
                    id: ToolCallId::from("call-1"),
                    text: text(&"x".repeat(20 * 1024)),
                },
            },
        ]
    }

    fn collect(messages: &[ModelMessage]) -> (u64, Vec<String>) {
        let transcript = ModelMessage::transcript_segments(messages).unwrap();
        let mut segments = Vec::new();
        transcript
            .emit(|index, text| {
                assert_eq!(index, segments.len() as u64 + 1);
                segments.push(text.to_owned());
            })
            .unwrap();
        (transcript.count(), segments)
    }

    #[test]
    fn the_transcript_reads_each_slice_once() {
        let mut sliced = 0;
        let file_text = |source: &str| {
            let encoded = serde_json::to_vec(source).unwrap();
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(&encoded).unwrap();
            sliced += encoded.len() as u64 - 2;
            Text::File {
                file: Arc::new(file),
                offset: 1,
                len: encoded.len() as u64 - 2,
            }
        };
        let raw = br#"{"type":"reasoning.encrypted","data":"opaque-canary"}"#;
        let mut reasoning = tempfile::tempfile().unwrap();
        reasoning.write_all(raw).unwrap();
        let journal = messages(
            file_text,
            OpaqueSource::File {
                file: Arc::new(reasoning),
                offset: 0,
                len: raw.len() as u64,
            },
        );
        let owned = messages(
            |source| Text::Owned(source.to_owned()),
            OpaqueSource::Owned(bytes::Bytes::from_static(raw)),
        );

        READ_BYTES.set(0);
        let (count, segments) = collect(&journal);
        assert_eq!(READ_BYTES.get(), sliced);
        assert_eq!(collect(&owned), (count, segments.clone()));
        assert_eq!(count, segments.len() as u64);
        assert!(segments.len() > 2);
        assert!(
            segments
                .iter()
                .all(|segment| segment.len() <= SEGMENT_BYTES)
        );
        let joined = segments.concat();
        assert!(!joined.contains("opaque-canary"));
        let parsed: serde_json::Value = serde_json::from_str(&joined).unwrap();
        assert_eq!(
            parsed[2]["reasoning_details"][0],
            format!("[opaque {} B]", raw.len())
        );
        assert_eq!(parsed[0]["content"], "owned question");
    }
}
