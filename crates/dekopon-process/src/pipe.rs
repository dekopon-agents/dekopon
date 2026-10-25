use std::{
    collections::VecDeque,
    io::Read,
    os::unix::net::UnixStream,
    sync::{
        atomic::{self, AtomicBool},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    time::Duration,
};

use crate::{LimitExceeded, RetainedBytes, TreeContext};

pub struct ChargedBytes {
    pub bytes: Vec<u8>,
    charges: Vec<RetainedBytes>,
}

impl ChargedBytes {
    pub fn into_parts(self) -> (Vec<u8>, Vec<RetainedBytes>) {
        (self.bytes, self.charges)
    }

    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            charges: Vec::new(),
        }
    }

    fn extend(
        &mut self,
        chunk: impl IntoIterator<Item = u8>,
        len: usize,
        budget: &TreeContext,
    ) -> Result<(), LimitExceeded> {
        self.charges.push(budget.retain(len as u64)?);
        self.bytes.extend(chunk);
        Ok(())
    }
}

pub const CHUNK_BYTES: usize = 4 * 1024;

const CAPACITY_CHUNKS: usize = 16;

pub const POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Eq, PartialEq)]
pub enum ReadOutcome {
    Bytes(Vec<u8>),
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum WriteOutcome {
    Accepted,
    ReaderGone,
}

#[derive(Debug)]
pub struct PipeWriter<C = ()> {
    sender: SyncSender<Vec<u8>>,
    context: C,
    observe: fn(usize, bool, &C),
}

impl<C> PipeWriter<C> {
    pub fn write(&mut self, bytes: &[u8]) -> WriteOutcome {
        for chunk in bytes.chunks(CHUNK_BYTES) {
            if self.sender.send(chunk.to_vec()).is_err() {
                (self.observe)(0, false, &self.context);
                return WriteOutcome::ReaderGone;
            }
            (self.observe)(chunk.len(), true, &self.context);
        }
        WriteOutcome::Accepted
    }
}

#[derive(Debug)]
enum Source {
    Channel(Receiver<Vec<u8>>),
    Socket(UnixStream),
    Fixed,
}

#[derive(Debug)]
pub struct PipeReader<C = ()> {
    source: Source,
    pending: VecDeque<u8>,
    ended: bool,
    context: C,
    observe: fn(usize, &C),
}

impl<C: Default> Default for PipeReader<C> {
    fn default() -> Self {
        Self::from_bytes(Vec::new(), C::default(), |_, _| {})
    }
}

impl<C> PipeReader<C> {
    /// Reads a provider's stdout; dropping the reader closes the socket, so the provider's next
    /// write finds its reader gone.
    pub fn from_socket(
        socket: UnixStream,
        context: C,
        observe: fn(usize, &C),
    ) -> std::io::Result<Self> {
        socket.set_read_timeout(Some(POLL))?;
        Ok(Self {
            source: Source::Socket(socket),
            pending: VecDeque::new(),
            ended: false,
            context,
            observe,
        })
    }

    pub fn from_bytes(bytes: Vec<u8>, context: C, observe: fn(usize, &C)) -> Self {
        Self {
            source: Source::Fixed,
            pending: bytes.into(),
            ended: true,
            context,
            observe,
        }
    }

    pub fn read(&mut self, budget: &TreeContext) -> Result<ReadOutcome, LimitExceeded> {
        self.read_unless(budget, &AtomicBool::new(false))
    }

    /// Reads as [`Self::read`] until `stopped` is set, then reports the end without ending the
    /// stream, so a later reader still finds whatever was not yet read.
    pub fn read_unless(
        &mut self,
        budget: &TreeContext,

        stopped: &AtomicBool,
    ) -> Result<ReadOutcome, LimitExceeded> {
        if !self.pending.is_empty() {
            let bytes: Vec<u8> = self
                .pending
                .drain(..self.pending.len().min(CHUNK_BYTES))
                .collect();
            (self.observe)(bytes.len(), &self.context);
            return Ok(ReadOutcome::Bytes(bytes));
        }
        loop {
            if self.ended {
                return Ok(ReadOutcome::End);
            }
            if stopped.load(atomic::Ordering::Relaxed) {
                return Ok(ReadOutcome::End);
            }
            if budget.is_cancelled() {
                return Err(LimitExceeded::Cancelled);
            }
            budget.check_deadline()?;
            let counted = match &mut self.source {
                Source::Fixed => continue,
                Source::Channel(receiver) => {
                    match receiver.recv_timeout(budget.remaining().min(POLL)) {
                        Ok(bytes) => bytes,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => {
                            self.ended = true;
                            continue;
                        }
                    }
                }
                Source::Socket(socket) => {
                    let mut bytes = vec![0; CHUNK_BYTES];
                    match socket.read(&mut bytes) {
                        Ok(0) => {
                            self.ended = true;
                            continue;
                        }
                        Ok(count) => {
                            bytes.truncate(count);
                            return Ok(ReadOutcome::Bytes(bytes));
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue;
                        }
                        Err(_) => {
                            self.ended = true;
                            continue;
                        }
                    }
                }
            };
            if !counted.is_empty() {
                (self.observe)(counted.len(), &self.context);
                return Ok(ReadOutcome::Bytes(counted));
            }
        }
    }

    pub fn read_line(
        &mut self,
        budget: &TreeContext,
    ) -> Result<Option<ChargedBytes>, LimitExceeded> {
        let mut line = ChargedBytes::new();
        loop {
            if let Some(offset) = self.pending.iter().position(|byte| *byte == b'\n') {
                line.extend(self.pending.drain(..offset), offset, budget)?;
                self.pending.pop_front();
                return Ok(Some(line));
            }
            let len = self.pending.len();
            line.extend(self.pending.drain(..), len, budget)?;
            match self.read(budget)? {
                ReadOutcome::Bytes(chunk) => self.pending.extend(chunk),
                ReadOutcome::End => return Ok((!line.bytes.is_empty()).then_some(line)),
            }
        }
    }
}

pub fn pipe<R, W>(
    reader_context: R,
    writer_context: W,
    read: fn(usize, &R),
    write: fn(usize, bool, &W),
) -> (PipeWriter<W>, PipeReader<R>) {
    let (sender, receiver) = sync_channel(CAPACITY_CHUNKS);
    (
        PipeWriter {
            sender,
            context: writer_context,
            observe: write,
        },
        PipeReader {
            source: Source::Channel(receiver),
            pending: VecDeque::new(),
            ended: false,
            context: reader_context,
            observe: read,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallBudget, Limits};

    #[test]
    fn a_line_read_leaves_the_rest_of_the_chunk_for_the_next_reader() {
        let budget = TreeContext::new(Limits::default(), CallBudget::new(32));
        let (mut writer, mut reader) = pipe((), (), |_, _| {}, |_, _, _| {});
        assert_eq!(writer.write(b"a\nbc"), WriteOutcome::Accepted);
        drop(writer);
        assert_eq!(
            reader
                .read_line(&budget)
                .expect("reads")
                .map(|line| line.bytes),
            Some(b"a".to_vec())
        );
        assert_eq!(
            reader.read(&budget).expect("reads"),
            ReadOutcome::Bytes(b"bc".to_vec())
        );
        assert_eq!(reader.read(&budget).expect("reads"), ReadOutcome::End);
    }

    #[test]
    fn a_growing_line_and_drain_are_charged_before_appending() {
        let budget = TreeContext::new(
            Limits {
                max_value_bytes: 12,
                ..Limits::default()
            },
            CallBudget::new(32),
        );
        let (mut writer, mut reader) = pipe((), (), |_, _| {}, |_, _, _| {});
        assert_eq!(writer.write(b"1234567890123456"), WriteOutcome::Accepted);
        drop(writer);
        assert!(matches!(
            reader.read_line(&budget),
            Err(LimitExceeded::ValueBytes { maximum: 12 })
        ));
        assert_eq!(budget.value_bytes(), 0);
    }

    #[test]
    fn a_read_past_the_deadline_fails_without_waiting() {
        let budget = TreeContext::new(
            Limits {
                timeout: Duration::ZERO,
                ..Limits::default()
            },
            CallBudget::new(32),
        );
        let (_writer, mut reader) = pipe((), (), |_, _| {}, |_, _, _| {});
        assert!(matches!(
            reader.read(&budget),
            Err(LimitExceeded::Deadline { .. })
        ));
    }

    #[test]
    fn a_writer_learns_the_reader_is_gone_instead_of_blocking() {
        let (mut writer, reader) = pipe((), (), |_, _| {}, |_, _, _| {});
        drop(reader);
        assert_eq!(writer.write(b"x"), WriteOutcome::ReaderGone);
    }
}
