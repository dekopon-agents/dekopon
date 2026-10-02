use std::{
    collections::VecDeque,
    io::Read,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    time::Duration,
};

use crate::{
    CapabilityInvoker, RetainedBytes,
    interp::telemetry::{self, StageMeter},
    limits::Budget,
    limits::LimitExceeded,
};

pub(crate) struct ChargedBytes {
    pub(crate) bytes: Vec<u8>,
    charges: Vec<RetainedBytes>,
}

impl ChargedBytes {
    pub(crate) fn into_parts(self) -> (Vec<u8>, Vec<RetainedBytes>) {
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
        budget: &Budget,
    ) -> Result<(), LimitExceeded> {
        self.charges.push(budget.charge_value_bytes(len as u64)?);
        self.bytes.extend(chunk);
        Ok(())
    }
}

pub(crate) const CHUNK_BYTES: usize = 4 * 1024;

const CAPACITY_CHUNKS: usize = 16;

pub(crate) const POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ReadOutcome {
    Bytes(Vec<u8>),
    End,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum WriteOutcome {
    Accepted,
    ReaderGone,
}

#[derive(Debug)]
pub(crate) struct PipeWriter {
    sender: SyncSender<Vec<u8>>,
    meter: Option<Arc<StageMeter>>,
}

impl PipeWriter {
    pub(crate) fn with_meter(mut self, meter: Arc<StageMeter>) -> Self {
        self.meter = Some(meter);
        self
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> WriteOutcome {
        for chunk in bytes.chunks(CHUNK_BYTES) {
            if self.sender.send(chunk.to_vec()).is_err() {
                telemetry::stage_write(0, false, self.meter.as_ref());
                return WriteOutcome::ReaderGone;
            }
            telemetry::stage_write(chunk.len(), true, self.meter.as_ref());
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
pub(crate) struct PipeReader {
    source: Source,
    pending: VecDeque<u8>,
    ended: bool,
    meter: Option<Arc<StageMeter>>,
}

impl Default for PipeReader {
    fn default() -> Self {
        Self::from_bytes(Vec::new())
    }
}

impl PipeReader {
    pub(crate) fn with_meter(mut self, meter: Arc<StageMeter>) -> Self {
        self.meter = Some(meter);
        self
    }

    /// Reads a provider's stdout; dropping the reader closes the socket, so the provider's next
    /// write finds its reader gone.
    pub(crate) fn from_socket(socket: UnixStream) -> std::io::Result<Self> {
        socket.set_read_timeout(Some(POLL))?;
        Ok(Self {
            source: Source::Socket(socket),
            pending: VecDeque::new(),
            ended: false,
            meter: None,
        })
    }

    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            source: Source::Fixed,
            pending: bytes.into(),
            ended: true,
            meter: None,
        }
    }

    pub(crate) fn read(
        &mut self,
        budget: &Budget,
        invoker: &dyn CapabilityInvoker,
    ) -> Result<ReadOutcome, LimitExceeded> {
        if !self.pending.is_empty() {
            let bytes: Vec<u8> = self
                .pending
                .drain(..self.pending.len().min(CHUNK_BYTES))
                .collect();
            telemetry::stage_read(bytes.len(), self.meter.as_ref());
            return Ok(ReadOutcome::Bytes(bytes));
        }
        loop {
            if self.ended {
                return Ok(ReadOutcome::End);
            }
            if let Source::Fixed = self.source {
                self.ended = true;
                continue;
            }
            if invoker.cancelled() {
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
                telemetry::stage_read(counted.len(), self.meter.as_ref());
                return Ok(ReadOutcome::Bytes(counted));
            }
        }
    }

    pub(crate) fn read_line(
        &mut self,
        budget: &Budget,
        invoker: &dyn CapabilityInvoker,
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
            match self.read_more(budget, invoker)? {
                true => {}
                false if line.bytes.is_empty() => return Ok(None),
                false => {
                    return Ok(Some(line));
                }
            }
        }
    }

    fn read_more(
        &mut self,
        budget: &Budget,
        invoker: &dyn CapabilityInvoker,
    ) -> Result<bool, LimitExceeded> {
        match self.read(budget, invoker)? {
            ReadOutcome::Bytes(chunk) => {
                self.pending.extend(chunk);
                Ok(true)
            }
            ReadOutcome::End => Ok(false),
        }
    }
}

pub(crate) fn pipe() -> (PipeWriter, PipeReader) {
    let (sender, receiver) = sync_channel(CAPACITY_CHUNKS);
    (
        PipeWriter {
            sender,
            meter: None,
        },
        PipeReader {
            source: Source::Channel(receiver),
            pending: VecDeque::new(),
            ended: false,
            meter: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::Limits;

    struct Idle;

    impl CapabilityInvoker for Idle {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }

        fn invoke(
            &self,
            _proposal: crate::CommandProposal,
            _streams: crate::Streams,
        ) -> crate::CapabilityCallResult {
            unreachable!("pipe tests never invoke")
        }
    }

    #[test]
    fn a_line_read_leaves_the_rest_of_the_chunk_for_the_next_reader() {
        let budget = Budget::start(Limits::default());
        let (mut writer, mut reader) = pipe();
        assert_eq!(writer.write(b"a\nbc"), WriteOutcome::Accepted);
        drop(writer);
        assert_eq!(
            reader
                .read_line(&budget, &Idle)
                .expect("reads")
                .map(|line| line.bytes),
            Some(b"a".to_vec())
        );
        assert_eq!(
            reader.read(&budget, &Idle).expect("reads"),
            ReadOutcome::Bytes(b"bc".to_vec())
        );
        assert_eq!(
            reader.read(&budget, &Idle).expect("reads"),
            ReadOutcome::End
        );
    }

    #[test]
    fn a_growing_line_and_drain_are_charged_before_appending() {
        let budget = Budget::start(Limits {
            max_value_bytes: 12,
            ..Limits::default()
        });
        let (mut writer, mut reader) = pipe();
        assert_eq!(writer.write(b"1234567890123456"), WriteOutcome::Accepted);
        drop(writer);
        assert!(matches!(
            reader.read_line(&budget, &Idle),
            Err(LimitExceeded::ValueBytes { maximum: 12 })
        ));
        assert_eq!(budget.value_bytes(), 0);
    }

    #[test]
    fn a_read_past_the_deadline_fails_without_waiting() {
        let budget = Budget::start(Limits {
            timeout: Duration::ZERO,
            ..Limits::default()
        });
        let (_writer, mut reader) = pipe();
        assert!(matches!(
            reader.read(&budget, &Idle),
            Err(LimitExceeded::Deadline { .. })
        ));
    }

    #[test]
    fn a_writer_learns_the_reader_is_gone_instead_of_blocking() {
        let (mut writer, reader) = pipe();
        drop(reader);
        assert_eq!(writer.write(b"x"), WriteOutcome::ReaderGone);
    }
}
