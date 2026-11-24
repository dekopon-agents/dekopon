use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

use crate::{
    DaemonError,
    private_file::{PrivateFileLock, TemporarySweep},
};

// Blocking: callers run every method inside `offload`; `&mut self` serializes replacements in
// the process and the sibling lock serializes them across processes.
#[derive(Debug)]
pub struct DurableFile {
    path: PathBuf,
    lock: PrivateFileLock,
    expected_uid: u32,
    max_bytes: usize,
}

impl DurableFile {
    #[expect(clippy::todo, reason = "skeleton: daemon/durable-replace")]
    pub fn open(
        path: &Path,
        expected_uid: u32,
        max_bytes: usize,
    ) -> Result<(Self, Vec<TemporarySweep>), DaemonError> {
        let _open = (path, expected_uid, max_bytes);
        todo!("skeleton: daemon/durable-replace")
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/durable-replace")]
    pub fn read(&self) -> Result<Option<Vec<u8>>, DaemonError> {
        let _read = (&self.lock, self.expected_uid, self.max_bytes);
        todo!("skeleton: daemon/durable-replace")
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/durable-replace")]
    pub fn replace_with(
        &mut self,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> Result<(), DaemonError> {
        let _write = write;
        todo!("skeleton: daemon/durable-replace")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "skeleton: daemon/durable-replace"]
    #[expect(clippy::todo, reason = "skeleton: daemon/durable-replace")]
    fn a_write_aborted_mid_replace_leaves_the_previous_record() {
        todo!("skeleton: daemon/durable-replace")
    }

    #[test]
    #[ignore = "skeleton: daemon/durable-replace"]
    #[expect(clippy::todo, reason = "skeleton: daemon/durable-replace")]
    fn a_read_after_an_outside_replace_returns_the_new_record() {
        todo!("skeleton: daemon/durable-replace")
    }

    #[test]
    #[ignore = "skeleton: daemon/durable-replace"]
    #[expect(clippy::todo, reason = "skeleton: daemon/durable-replace")]
    fn a_record_over_the_maximum_is_refused_and_the_previous_kept() {
        todo!("skeleton: daemon/durable-replace")
    }

    #[test]
    #[ignore = "skeleton: daemon/durable-replace"]
    #[expect(clippy::todo, reason = "skeleton: daemon/durable-replace")]
    fn reopening_after_a_killed_writer_sweeps_its_temporary() {
        todo!("skeleton: daemon/durable-replace")
    }
}
