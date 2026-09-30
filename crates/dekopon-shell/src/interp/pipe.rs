use std::{
    sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    time::Duration,
};

use crate::{
    CapabilityInvoker,
    limits::{Budget, LimitExceeded},
};

pub(super) const STACK_BYTES: usize = 2 * 1024 * 1024;
pub(super) const CHUNK_BYTES: usize = 4096;
const CAPACITY: usize = 8;

pub(super) struct Chunk(Vec<u8>);
pub(super) enum ReadOutcome {
    Bytes(Chunk),
    End,
}
#[must_use]
pub(super) enum WriteOutcome {
    Accepted,
    ReaderGone,
}
pub(super) struct PipeReader(Receiver<Chunk>);
pub(super) struct PipeWriter(SyncSender<Chunk>);
pub(super) struct Pipe {
    pub reader: PipeReader,
    pub writer: PipeWriter,
}

impl Pipe {
    pub fn new() -> Self {
        let (writer, reader) = sync_channel(CAPACITY);
        Self {
            reader: PipeReader(reader),
            writer: PipeWriter(writer),
        }
    }
}
impl Chunk {
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}
impl PipeReader {
    pub fn read(
        &mut self,
        budget: &mut Budget,
        invoker: &dyn CapabilityInvoker,
    ) -> Result<ReadOutcome, LimitExceeded> {
        loop {
            budget.charge_step_with(invoker)?;
            match self
                .0
                .recv_timeout(budget.remaining().min(Duration::from_secs(1)))
            {
                Ok(chunk) => return Ok(ReadOutcome::Bytes(chunk)),
                Err(RecvTimeoutError::Disconnected) => return Ok(ReadOutcome::End),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}
impl PipeWriter {
    pub fn write(
        &mut self,
        bytes: &[u8],
        budget: &mut Budget,
        invoker: &dyn CapabilityInvoker,
    ) -> Result<WriteOutcome, LimitExceeded> {
        for chunk in bytes.chunks(CHUNK_BYTES) {
            budget.charge_step_with(invoker)?;
            if self.0.send(Chunk(chunk.to_vec())).is_err() {
                return Ok(WriteOutcome::ReaderGone);
            }
        }
        Ok(WriteOutcome::Accepted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CapabilityCallResult, CommandProposal, limits::Limits};
    struct Invoker;
    impl CapabilityInvoker for Invoker {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }
        fn is_granted(&self, _: &str) -> bool {
            false
        }
        fn invoke(&self, _: CommandProposal) -> CapabilityCallResult {
            CapabilityCallResult::NotFound
        }
    }

    #[test]
    fn empty_writes_are_not_end_of_input() {
        let Pipe {
            mut reader,
            mut writer,
        } = Pipe::new();
        let mut budget = Budget::start(Limits::default());
        assert!(matches!(
            writer.write(b"", &mut budget, &Invoker),
            Ok(WriteOutcome::Accepted)
        ));
        assert!(matches!(
            writer.write(b"hi", &mut budget, &Invoker),
            Ok(WriteOutcome::Accepted)
        ));
        drop(writer);
        assert!(
            matches!(reader.read(&mut budget, &Invoker), Ok(ReadOutcome::Bytes(chunk)) if chunk.bytes() == b"hi")
        );
        assert!(matches!(
            reader.read(&mut budget, &Invoker),
            Ok(ReadOutcome::End)
        ));
    }
    #[test]
    fn closing_a_reader_releases_a_flooding_producer_and_joins_it() {
        let Pipe {
            mut reader,
            mut writer,
        } = Pipe::new();
        let producer = std::thread::spawn(move || {
            let mut budget = Budget::start(Limits::default());
            loop {
                match writer
                    .write(&[b'x'; CHUNK_BYTES], &mut budget, &Invoker)
                    .expect("within limits")
                {
                    WriteOutcome::Accepted => {}
                    WriteOutcome::ReaderGone => return,
                }
            }
        });
        let mut budget = Budget::start(Limits::default());
        assert!(matches!(
            reader.read(&mut budget, &Invoker),
            Ok(ReadOutcome::Bytes(_))
        ));
        drop(reader);
        producer.join().expect("producer stopped");
    }
}
