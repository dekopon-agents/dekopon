use std::future::Future;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Duration;

use dekopon_broker_protocol::Streams;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::Notify;
use tokio::time::Instant;
use wasmtime::component::Resource;

use crate::StoreState;
use crate::bindings::dekopon::stdio::streams as wit;

// Lowered before the read buffer is allocated, so a guest cannot ask the host for 4 GiB.
pub const MAX_READ_BYTES: u32 = 256 * 1024;
pub const MAX_STDERR_BYTES: usize = 64 * 1024;
pub const STDERR_TRUNCATION_MARKER: &str = "\u{2026}[truncated]";
pub const ZERO_FAILURE_STATUS_NOTE: &str =
    "provider reported failure with exit status 0; reported as status 1\n";
// Handles live in the host table, outside the guest's memory limit, so a guest may not mint them
// without bound.
const MAX_LIVE_HANDLES: u8 = 16;
const TRACE_PREFIX_BYTES: usize = 4096;

#[derive(Debug, Default)]
pub(crate) struct StreamTrace {
    pub(crate) bytes: u64,
    prefix: Vec<u8>,
}

impl StreamTrace {
    fn record(&mut self, bytes: &[u8]) {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        let remaining = TRACE_PREFIX_BYTES.saturating_sub(self.prefix.len());
        self.prefix
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    }

    pub(crate) fn prefix(&self) -> String {
        let mut text = String::from_utf8_lossy(&self.prefix).into_owned();
        if text.len() > TRACE_PREFIX_BYTES {
            text.truncate(text.floor_char_boundary(TRACE_PREFIX_BYTES));
        }
        text
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StdioTrap {
    #[error("the provider held more than {MAX_LIVE_HANDLES} stdio handles at once")]
    TooManyHandles,
    #[error("the provider asked to read zero bytes, which would read as end of input")]
    ZeroRead,
}

#[derive(Debug, thiserror::Error)]
pub enum StdioAdmissionError {
    #[error("a stdio descriptor is not a stream socket")]
    NotStreamSocket,
    #[error("a stdio descriptor could not be inspected or adopted")]
    Io(#[from] std::io::Error),
}

pub enum ReaderResource {
    Stdin,
}
pub struct WriterResource;

#[derive(Debug, Default)]
pub(crate) struct StdioState {
    stdin: Option<UnixStream>,
    stdout: Option<UnixStream>,
    stderr: String,
    stderr_truncated: bool,
    pub(crate) stdin_trace: StreamTrace,
    pub(crate) stdout_trace: StreamTrace,
    pub(crate) stderr_trace: StreamTrace,
    handles: u8,
    pub(crate) clock: WorkClock,
}

impl StdioState {
    pub(crate) fn none() -> Self {
        Self::default()
    }

    pub(crate) fn invoke(streams: Option<Streams>) -> Result<Self, StdioAdmissionError> {
        let Some(streams) = streams else {
            return Ok(Self::none());
        };
        Ok(Self {
            stdin: streams.stdin.map(admit).transpose()?,
            stdout: Some(admit(streams.stdout)?),
            ..Self::default()
        })
    }

    pub(crate) fn take_stderr(&mut self) -> String {
        std::mem::take(&mut self.stderr)
    }

    pub(crate) fn append_stderr(&mut self, text: &str) {
        self.stderr_trace.record(text.as_bytes());
        if self.stderr_truncated {
            return;
        }
        if self.stderr.len() + text.len() <= MAX_STDERR_BYTES {
            self.stderr.push_str(text);
            return;
        }
        let keep = MAX_STDERR_BYTES - STDERR_TRUNCATION_MARKER.len();
        if self.stderr.len() > keep {
            self.stderr.truncate(self.stderr.floor_char_boundary(keep));
        } else {
            let room = keep - self.stderr.len();
            self.stderr
                .push_str(&text[..text.floor_char_boundary(room)]);
        }
        self.stderr.push_str(STDERR_TRUNCATION_MARKER);
        self.stderr_truncated = true;
    }

    fn mint(&mut self) -> wasmtime::Result<()> {
        if self.handles >= MAX_LIVE_HANDLES {
            return Err(StdioTrap::TooManyHandles.into());
        }
        self.handles += 1;
        Ok(())
    }

    // The note is its own line and survives a full stderr, so the prefix gives way to it.
    pub(crate) fn note_zero_status(&mut self) {
        let room = MAX_STDERR_BYTES - ZERO_FAILURE_STATUS_NOTE.len() - 1;
        if self.stderr.len() > room {
            let keep = room - STDERR_TRUNCATION_MARKER.len();
            self.stderr.truncate(self.stderr.floor_char_boundary(keep));
            self.stderr.push_str(STDERR_TRUNCATION_MARKER);
        }
        if !self.stderr.is_empty() && !self.stderr.ends_with('\n') {
            self.stderr.push('\n');
            self.stderr_trace.record(b"\n");
        }
        self.stderr.push_str(ZERO_FAILURE_STATUS_NOTE);
        self.stderr_trace
            .record(ZERO_FAILURE_STATUS_NOTE.as_bytes());
    }

    fn release(&mut self) {
        self.handles = self.handles.saturating_sub(1);
    }

    // The peer sees end of output only once both ends are dropped here.
    pub(crate) fn close(&mut self) {
        self.stdin = None;
        self.stdout = None;
    }
}

// The gateway is a trusted peer, so this is hygiene rather than a boundary.
pub(crate) fn admit(descriptor: OwnedFd) -> Result<UnixStream, StdioAdmissionError> {
    let stat = rustix::fs::fstat(&descriptor).map_err(std::io::Error::from)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Socket {
        return Err(StdioAdmissionError::NotStreamSocket);
    }
    let kind = rustix::net::sockopt::socket_type(&descriptor).map_err(std::io::Error::from)?;
    if kind != rustix::net::SocketType::STREAM {
        return Err(StdioAdmissionError::NotStreamSocket);
    }
    let stream = std::os::unix::net::UnixStream::from(descriptor);
    stream.set_nonblocking(true)?;
    Ok(UnixStream::from_std(stream)?)
}

// A provider's deadline measures its own work, so time parked on a stdio peer is not charged.
#[derive(Clone, Debug, Default)]
pub(crate) struct WorkClock(Arc<WorkClockInner>);

#[derive(Debug, Default)]
struct WorkClockInner {
    parked: Mutex<Parked>,
    changed: Notify,
}

#[derive(Debug, Default)]
struct Parked {
    total: Duration,
    since: Option<Instant>,
}

pub(crate) struct ParkGuard(WorkClock);

impl Drop for ParkGuard {
    fn drop(&mut self) {
        let inner = &self.0.0;
        let mut parked = inner.parked.lock();
        if let Some(since) = parked.since.take() {
            parked.total += since.elapsed();
        }
        drop(parked);
        inner.changed.notify_one();
    }
}

impl WorkClock {
    fn park(&self) -> ParkGuard {
        self.0.parked.lock().since = Some(Instant::now());
        self.0.changed.notify_one();
        ParkGuard(self.clone())
    }

    fn charged(&self, started: Instant) -> (Duration, bool) {
        let parked = self.0.parked.lock();
        let current = parked
            .since
            .map(|since| since.elapsed())
            .unwrap_or_default();
        (
            started.elapsed().saturating_sub(parked.total + current),
            parked.since.is_some(),
        )
    }

    // A parked guest is never timed out here; its peer closing is what releases it.
    pub(crate) async fn run<F: Future>(&self, budget: Duration, operation: F) -> Option<F::Output> {
        let started = Instant::now();
        tokio::pin!(operation);
        loop {
            let (charged, parked) = self.charged(started);
            let changed = self.0.changed.notified();
            if parked {
                tokio::select! {
                    biased;
                    output = &mut operation => return Some(output),
                    () = changed => {}
                }
                continue;
            }
            let remaining = budget.checked_sub(charged).filter(|left| !left.is_zero())?;
            tokio::select! {
                biased;
                output = &mut operation => return Some(output),
                () = changed => {}
                () = tokio::time::sleep(remaining) => {}
            }
        }
    }
}

impl StoreState {
    async fn read_stdin(&mut self, length: usize) -> Vec<u8> {
        let Some(stream) = self.stdio.stdin.as_mut() else {
            return Vec::new();
        };
        let mut buffer = vec![0; length];
        let parked = self.stdio.clock.park();
        let read = stream.read(&mut buffer).await;
        drop(parked);
        match read {
            Ok(count) => {
                buffer.truncate(count);
                self.stdio.stdin_trace.record(&buffer);
                if count == 0 {
                    self.stdio.stdin = None;
                }
                buffer
            }
            Err(_) => {
                self.stdio.stdin = None;
                Vec::new()
            }
        }
    }

    async fn write_stdout(&mut self, bytes: &[u8]) -> Result<(), wit::WriteError> {
        let Some(stream) = self.stdio.stdout.as_mut() else {
            return Err(wit::WriteError::Closed);
        };
        let parked = self.stdio.clock.park();
        let written = stream.write_all(bytes).await;
        drop(parked);
        if written.is_err() {
            self.stdio.stdout = None;
            return Err(wit::WriteError::Closed);
        }
        self.stdio.stdout_trace.record(bytes);
        Ok(())
    }
}

impl wit::HostReader for StoreState {
    async fn read(
        &mut self,
        resource: Resource<ReaderResource>,
        max: u32,
    ) -> wasmtime::Result<Vec<u8>> {
        self.table.get(&resource)?;
        if max == 0 {
            return Err(StdioTrap::ZeroRead.into());
        }
        let length = usize::try_from(max.min(MAX_READ_BYTES))?;
        match self.table.get(&resource)? {
            ReaderResource::Stdin => Ok(self.read_stdin(length).await),
        }
    }

    async fn drop(&mut self, resource: Resource<ReaderResource>) -> wasmtime::Result<()> {
        self.table.delete(resource)?;
        self.stdio.release();
        Ok(())
    }
}

impl wit::HostWriter for StoreState {
    async fn write(
        &mut self,
        resource: Resource<WriterResource>,
        bytes: Vec<u8>,
    ) -> wasmtime::Result<Result<(), wit::WriteError>> {
        self.table.get(&resource)?;
        Ok(self.write_stdout(&bytes).await)
    }

    async fn drop(&mut self, resource: Resource<WriterResource>) -> wasmtime::Result<()> {
        self.table.delete(resource)?;
        self.stdio.release();
        Ok(())
    }
}

impl wit::Host for StoreState {
    async fn stdin(&mut self) -> wasmtime::Result<Option<Resource<ReaderResource>>> {
        if self.stdio.stdin.is_none() {
            return Ok(None);
        }
        self.stdio.mint()?;
        Ok(Some(self.table.push(ReaderResource::Stdin)?))
    }

    async fn stdout(&mut self) -> wasmtime::Result<Resource<WriterResource>> {
        self.stdio.mint()?;
        Ok(self.table.push(WriterResource)?)
    }

    async fn write_stderr(&mut self, text: String) -> wasmtime::Result<()> {
        self.stdio.append_stderr(&text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        MAX_STDERR_BYTES, STDERR_TRUNCATION_MARKER, StdioState, StreamTrace, WorkClock,
        ZERO_FAILURE_STATUS_NOTE,
    };

    #[test]
    fn stream_telemetry_counts_all_bytes_and_keeps_only_the_first_four_kib() {
        let mut stream = StreamTrace::default();
        stream.record(b"first");
        stream.record(&vec![b'x'; 8192]);
        assert_eq!(stream.bytes, 8197);
        assert_eq!(stream.prefix().len(), 4096);
        assert!(stream.prefix().starts_with("first"));
        stream.record(b"last");
        assert_eq!(stream.bytes, 8201);
        assert!(!stream.prefix().contains("last"));
        let mut split_utf8 = StreamTrace::default();
        split_utf8.record(&vec![b'x'; 4095]);
        split_utf8.record(&[0xc3]);
        assert!(split_utf8.prefix().len() <= 4096);
    }

    #[test]
    fn the_zero_status_note_fits_after_a_truncated_stderr() {
        let mut state = StdioState::none();
        state.append_stderr(&"x".repeat(2 * MAX_STDERR_BYTES));
        state.note_zero_status();
        let stderr = state.take_stderr();
        assert!(stderr.len() <= MAX_STDERR_BYTES);
        assert!(stderr.ends_with(&format!(
            "{STDERR_TRUNCATION_MARKER}\n{ZERO_FAILURE_STATUS_NOTE}"
        )));
        assert_eq!(stderr.matches(STDERR_TRUNCATION_MARKER).count(), 1);
    }

    #[test]
    fn stderr_keeps_a_bounded_prefix_and_marks_the_cut_once() {
        let mut state = StdioState::none();
        state.append_stderr("first line\n");
        for _ in 0..100 {
            state.append_stderr(&"é".repeat(1000));
        }
        let stderr = state.take_stderr();
        assert!(stderr.len() <= MAX_STDERR_BYTES);
        assert!(stderr.starts_with("first line\n"));
        assert!(stderr.ends_with(STDERR_TRUNCATION_MARKER));
        assert_eq!(stderr.matches(STDERR_TRUNCATION_MARKER).count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn parked_time_is_not_charged_to_the_work_budget() {
        let clock = WorkClock::default();
        let parked_clock = clock.clone();
        let outcome = clock
            .run(Duration::from_secs(1), async move {
                let guard = parked_clock.park();
                tokio::time::sleep(Duration::from_secs(60)).await;
                drop(guard);
                tokio::time::sleep(Duration::from_millis(500)).await;
                "finished"
            })
            .await;
        assert_eq!(outcome, Some("finished"));
    }

    #[tokio::test(start_paused = true)]
    async fn unparked_work_still_times_out() {
        let clock = WorkClock::default();
        let outcome = clock
            .run(
                Duration::from_secs(1),
                tokio::time::sleep(Duration::from_secs(2)),
            )
            .await;
        assert_eq!(outcome, None);
    }
}
