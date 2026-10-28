use std::{
    collections::VecDeque,
    fs::File,
    future::Future,
    io,
    os::unix::fs::FileExt,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
};

use bytes::{Bytes, BytesMut};
use http_body::{Body, Frame, SizeHint};

#[derive(Debug, thiserror::Error)]
#[error("journal slice read failed: {kind:?}")]
pub(crate) struct ReadFailure {
    pub kind: io::ErrorKind,
}

const BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub enum OpaqueSource {
    Owned(Bytes),
    File {
        file: Arc<File>,
        offset: u64,
        len: u64,
    },
}

impl OpaqueSource {
    pub fn owned(bytes: &[u8]) -> Self {
        Self::Owned(Bytes::copy_from_slice(bytes))
    }

    pub const fn len(&self) -> u64 {
        match self {
            Self::Owned(bytes) => bytes.len() as u64,
            Self::File { len, .. } => *len,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for OpaqueSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpaqueSource")
            .field("bytes", &self.len())
            .finish_non_exhaustive()
    }
}

impl PartialEq for OpaqueSource {
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
            (Self::Owned(_), Self::File { .. }) | (Self::File { .. }, Self::Owned(_)) => false,
        }
    }
}

#[derive(Clone)]
pub enum Chunk {
    Literal(Bytes),
    File {
        file: Arc<File>,
        offset: u64,
        len: u64,
    },
    Opaque(OpaqueSource),
}

impl Chunk {
    fn len(&self) -> u64 {
        match self {
            Self::Literal(bytes) => bytes.len() as u64,
            Self::File { len, .. } => *len,
            Self::Opaque(source) => source.len(),
        }
    }
}

struct ReadChunk {
    file: Arc<File>,
    offset: u64,
    remaining: u64,
    bytes: Bytes,
}

pub struct SliceBody {
    chunks: VecDeque<Chunk>,
    active: Option<ReadChunk>,
    pending: Option<tokio::task::JoinHandle<io::Result<ReadChunk>>>,
    reusable: Bytes,
    remaining: u64,
}

impl SliceBody {
    pub fn new(chunks: &[Chunk]) -> io::Result<Self> {
        let remaining = chunks.iter().try_fold(0u64, |total, chunk| {
            total.checked_add(chunk.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "request body length overflow")
            })
        })?;
        Ok(Self {
            chunks: chunks.iter().cloned().collect(),
            active: None,
            pending: None,
            reusable: Bytes::new(),
            remaining,
        })
    }

    fn fail(&mut self, error: io::Error) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.chunks.clear();
        self.active = None;
        self.remaining = 0;
        Poll::Ready(Some(Err(error)))
    }
}

impl Body for SliceBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = self.get_mut();
        loop {
            if let Some(pending) = &mut this.pending {
                let result = ready!(Pin::new(pending).poll(cx));
                this.pending = None;
                let mut read = match result {
                    Ok(Ok(read)) => read,
                    Ok(Err(error)) => return this.fail(error),
                    Err(error) => return this.fail(io::Error::other(error)),
                };
                let bytes = std::mem::take(&mut read.bytes);
                this.remaining -= bytes.len() as u64;
                this.reusable = bytes.clone();
                if read.remaining > 0 {
                    this.active = Some(read);
                }
                return Poll::Ready(Some(Ok(Frame::data(bytes))));
            }
            if let Some(mut read) = this.active.take() {
                read.bytes = std::mem::take(&mut this.reusable);
                let pending = tokio::task::spawn_blocking(move || read_chunk(read));
                this.pending = Some(pending);
                continue;
            }
            match this.chunks.pop_front() {
                Some(Chunk::Literal(bytes) | Chunk::Opaque(OpaqueSource::Owned(bytes))) => {
                    if !bytes.is_empty() {
                        this.remaining -= bytes.len() as u64;
                        return Poll::Ready(Some(Ok(Frame::data(bytes))));
                    }
                }
                Some(
                    Chunk::File { file, offset, len }
                    | Chunk::Opaque(OpaqueSource::File { file, offset, len }),
                ) => {
                    if len > 0 {
                        this.active = Some(ReadChunk {
                            file,
                            offset,
                            remaining: len,
                            bytes: Bytes::new(),
                        });
                    }
                }
                None => return Poll::Ready(None),
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }
}

fn read_chunk(mut read: ReadChunk) -> io::Result<ReadChunk> {
    let count = read.remaining.min(BUFFER_BYTES as u64) as usize;
    let mut buffer = std::mem::take(&mut read.bytes)
        .try_into_mut()
        .unwrap_or_else(|_| BytesMut::with_capacity(BUFFER_BYTES));
    buffer.resize(count, 0);
    read.file
        .read_exact_at(&mut buffer, read.offset)
        .map_err(|error| io::Error::new(error.kind(), ReadFailure { kind: error.kind() }))?;
    read.offset = read.offset.checked_add(count as u64).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "journal slice offset overflow")
    })?;
    read.remaining -= count as u64;
    read.bytes = buffer.freeze();
    Ok(read)
}

#[cfg(test)]
mod tests;

struct ByteCounter(usize);

impl io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "literal length overflow")
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct JsonBody {
    chunks: Vec<Chunk>,
    literal: Vec<u8>,
}

impl JsonBody {
    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.literal.reserve_exact(bytes.len());
        self.literal.extend_from_slice(bytes);
    }

    pub(crate) fn value(
        &mut self,
        value: &(impl serde::Serialize + ?Sized),
    ) -> Result<(), serde_json::Error> {
        let mut count = ByteCounter(0);
        serde_json::to_writer(&mut count, value)?;
        self.literal.reserve_exact(count.0);
        serde_json::to_writer(&mut self.literal, value)
    }

    pub(crate) fn text(&mut self, text: &crate::text::Text) -> Result<(), serde_json::Error> {
        self.flush();
        text.chunks(&mut self.chunks)
    }

    pub(crate) fn joined(&mut self, texts: &[crate::text::Text]) -> Result<(), serde_json::Error> {
        self.raw(b"\"");
        for text in texts {
            match text {
                crate::text::Text::Owned(text) => {
                    let bytes = scalar_bytes(text)?;
                    self.raw(&bytes[1..bytes.len() - 1]);
                }
                crate::text::Text::File { file, offset, len } => {
                    self.flush();
                    self.chunks.push(Chunk::File {
                        file: Arc::clone(file),
                        offset: *offset,
                        len: *len,
                    });
                }
                crate::text::Text::Opaque(_) => {
                    return Err(serde_json::Error::io(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "opaque reasoning is not visible text",
                    )));
                }
            }
        }
        self.raw(b"\"");
        Ok(())
    }

    pub(crate) fn source(&mut self, source: &OpaqueSource) {
        self.flush();
        self.chunks.push(Chunk::Opaque(source.clone()));
    }

    pub(crate) fn opaque(
        &mut self,
        value: &(impl serde::Serialize + ?Sized),
    ) -> Result<(), serde_json::Error> {
        self.flush();
        self.chunks
            .push(Chunk::Opaque(OpaqueSource::Owned(scalar_bytes(value)?)));
        Ok(())
    }

    fn flush(&mut self) {
        if !self.literal.is_empty() {
            self.chunks.push(Chunk::Literal(Bytes::from(std::mem::take(
                &mut self.literal,
            ))));
        }
    }

    pub(crate) fn finish(mut self) -> Vec<Chunk> {
        self.flush();
        self.chunks
    }
}

#[cfg(test)]
pub(crate) async fn collect(chunks: &[Chunk]) -> io::Result<Vec<u8>> {
    let mut body = SliceBody::new(chunks)?;
    let mut bytes = Vec::new();
    while let Some(frame) =
        futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
    {
        bytes.extend_from_slice(&frame?.into_data().expect("data frame"));
    }
    Ok(bytes)
}

pub(crate) fn scalar_bytes(
    value: &(impl serde::Serialize + ?Sized),
) -> Result<Bytes, serde_json::Error> {
    let mut body = JsonBody::default();
    body.value(value)?;
    Ok(Bytes::from(body.literal))
}
