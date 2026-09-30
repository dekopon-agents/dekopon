use std::{
    collections::VecDeque,
    sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    time::Duration,
};

use crate::{CapabilityInvoker, limits::Budget, limits::LimitExceeded};

const CHUNK_BYTES: usize = 4 * 1024;

const CAPACITY_CHUNKS: usize = 16;

const POLL: Duration = Duration::from_secs(1);

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
}

impl PipeWriter {
    pub(crate) fn write(&mut self, bytes: &[u8]) -> WriteOutcome {
        for chunk in bytes.chunks(CHUNK_BYTES) {
            if self.sender.send(chunk.to_vec()).is_err() {
                return WriteOutcome::ReaderGone;
            }
        }
        WriteOutcome::Accepted
    }
}

#[derive(Debug)]
enum Source {
    Channel(Receiver<Vec<u8>>),
    Fixed,
}

#[derive(Debug)]
pub(crate) struct PipeReader {
    source: Source,
    pending: VecDeque<u8>,
    ended: bool,
}

impl Default for PipeReader {
    fn default() -> Self {
        Self::from_bytes(Vec::new())
    }
}

impl PipeReader {
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            source: Source::Fixed,
            pending: bytes.into(),
            ended: true,
        }
    }

    pub(crate) fn read(
        &mut self,
        budget: &Budget,
        invoker: &dyn CapabilityInvoker,
    ) -> Result<ReadOutcome, LimitExceeded> {
        if !self.pending.is_empty() {
            let bytes: Vec<u8> = self.pending.drain(..).collect();
            return Ok(ReadOutcome::Bytes(bytes));
        }
        loop {
            if self.ended {
                return Ok(ReadOutcome::End);
            }
            let Source::Channel(receiver) = &self.source else {
                self.ended = true;
                continue;
            };
            if invoker.cancelled() {
                return Err(LimitExceeded::Cancelled);
            }
            budget.check_deadline()?;
            let wait = budget.remaining().min(POLL);
            match receiver.recv_timeout(wait) {
                Ok(bytes) if bytes.is_empty() => {}
                Ok(bytes) => {
                    return Ok(ReadOutcome::Bytes(bytes));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.ended = true,
            }
        }
    }

    pub(crate) fn read_line(
        &mut self,
        budget: &Budget,
        invoker: &dyn CapabilityInvoker,
    ) -> Result<Option<Vec<u8>>, LimitExceeded> {
        let mut line = Vec::new();
        loop {
            if let Some(offset) = self.pending.iter().position(|byte| *byte == b'\n') {
                line.extend(self.pending.drain(..offset));
                self.pending.pop_front();
                return Ok(Some(line));
            }
            line.extend(self.pending.drain(..));
            match self.read_more(budget, invoker)? {
                true => {}
                false if line.is_empty() => return Ok(None),
                false => {
                    return Ok(Some(line));
                }
            }
        }
    }

    pub(crate) fn drain(
        &mut self,
        budget: &Budget,
        invoker: &dyn CapabilityInvoker,
    ) -> Result<Vec<u8>, LimitExceeded> {
        let mut bytes = Vec::new();
        while let ReadOutcome::Bytes(chunk) = self.read(budget, invoker)? {
            bytes.extend(chunk);
        }
        Ok(bytes)
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
        PipeWriter { sender },
        PipeReader {
            source: Source::Channel(receiver),
            pending: VecDeque::new(),
            ended: false,
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

        fn invoke(&self, _proposal: crate::CommandProposal) -> crate::CapabilityCallResult {
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
            reader.read_line(&budget, &Idle).expect("reads"),
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
