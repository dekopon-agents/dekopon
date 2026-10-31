use std::io;
use std::num::NonZeroU8;

use super::Failure;

#[cfg(target_arch = "wasm32")]
use crate::export_bindings::dekopon::stdio::streams;

const BROKEN_PIPE: NonZeroU8 = NonZeroU8::new(141).expect("141 is nonzero");

pub struct Stdin(Source);

enum Source {
    #[cfg(target_arch = "wasm32")]
    Guest(streams::Reader),
    #[cfg(not(target_arch = "wasm32"))]
    Native(Box<dyn io::Read>),
}

#[must_use]
pub fn stdin() -> Option<Stdin> {
    #[cfg(target_arch = "wasm32")]
    {
        streams::stdin().map(|reader| Stdin(Source::Guest(reader)))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::take_stdin().map(|reader| Stdin(Source::Native(reader)))
    }
}

impl Stdin {
    #[cfg(target_arch = "wasm32")]
    pub(crate) const fn from_guest(reader: streams::Reader) -> Self {
        Self(Source::Guest(reader))
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn into_guest(self) -> streams::Reader {
        let Source::Guest(reader) = self.0;
        reader
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn from_native(reader: Box<dyn io::Read>) -> Self {
        Self(Source::Native(reader))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn into_native(self) -> Box<dyn io::Read> {
        let Source::Native(reader) = self.0;
        reader
    }

    #[must_use]
    pub fn lines(self) -> io::Lines<io::BufReader<Self>> {
        io::BufRead::lines(io::BufReader::new(self))
    }
}

impl io::Read for Stdin {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        match &mut self.0 {
            #[cfg(target_arch = "wasm32")]
            Source::Guest(reader) => {
                let bytes = reader.read(u32::try_from(buffer.len()).unwrap_or(u32::MAX));
                let count = bytes.len().min(buffer.len());
                buffer[..count].copy_from_slice(&bytes[..count]);
                Ok(count)
            }
            #[cfg(not(target_arch = "wasm32"))]
            Source::Native(reader) => reader.read(buffer),
        }
    }
}

pub struct Stdout {
    sink: Sink,
    closed: bool,
}

enum Sink {
    #[cfg(target_arch = "wasm32")]
    Guest(streams::Writer),
    #[cfg(not(target_arch = "wasm32"))]
    Native(Box<dyn io::Write>),
}

impl Stdout {
    #[cfg(target_arch = "wasm32")]
    pub(super) fn open() -> Self {
        Self {
            sink: Sink::Guest(streams::stdout()),
            closed: false,
        }
    }

    #[must_use]
    pub const fn closed(&self) -> bool {
        self.closed
    }
}

impl io::Write for Stdout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let written = match &mut self.sink {
            #[cfg(target_arch = "wasm32")]
            Sink::Guest(writer) => writer.write(bytes).is_ok(),
            #[cfg(not(target_arch = "wasm32"))]
            Sink::Native(writer) => writer.write_all(bytes).is_ok(),
        };
        if !written {
            self.closed = true;
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct Exit {
    status: NonZeroU8,
    message: String,
}

impl<F: Failure> From<&F> for Exit {
    fn from(failure: &F) -> Self {
        Self {
            status: failure.code().status(),
            message: failure.to_string(),
        }
    }
}

pub(super) fn exit(outcome: Result<(), Exit>, out: &Stdout) -> Result<(), NonZeroU8> {
    if out.closed {
        return Err(BROKEN_PIPE);
    }
    outcome.map_err(|exit| {
        write_stderr(&format!("{}\n", exit.message));
        exit.status
    })
}

#[cfg(target_arch = "wasm32")]
fn write_stderr(text: &str) {
    streams::write_stderr(text);
}

#[cfg(not(target_arch = "wasm32"))]
fn write_stderr(text: &str) {
    native::append_stderr(text);
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn take_native_stdin() -> Option<Box<dyn io::Read>> {
    native::take_stdin()
}

#[cfg(not(target_arch = "wasm32"))]
pub struct NativeStdio {
    pub stdin: Option<Box<dyn io::Read>>,
    pub stdout: Box<dyn io::Write>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeExit {
    pub status: u8,
    pub stderr: String,
}

#[cfg(not(target_arch = "wasm32"))]
pub fn invoke_native<P: super::Provider>(
    capability: &str,
    input: &str,
    stdio: NativeStdio,
) -> NativeExit {
    let _installed = native::install(stdio.stdin);
    let mut out = Stdout {
        sink: Sink::Native(stdio.stdout),
        closed: false,
    };
    let outcome = super::dispatch::<P>(capability, input, &mut out);
    let status = exit(outcome, &out).map_or_else(NonZeroU8::get, |()| 0);
    NativeExit {
        status,
        stderr: native::take_stderr(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::cell::RefCell;
    use std::io;

    struct State {
        stdin: Option<Box<dyn io::Read>>,
        stderr: String,
    }

    thread_local! {
        static CURRENT: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    pub(super) struct Installed(Option<State>);

    impl Drop for Installed {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|current| *current.borrow_mut() = previous);
        }
    }

    pub(super) fn install(stdin: Option<Box<dyn io::Read>>) -> Installed {
        let state = State {
            stdin,
            stderr: String::new(),
        };
        Installed(CURRENT.with(|current| current.replace(Some(state))))
    }

    pub(super) fn take_stdin() -> Option<Box<dyn io::Read>> {
        CURRENT.with(|current| current.borrow_mut().as_mut()?.stdin.take())
    }

    pub(super) fn append_stderr(text: &str) {
        CURRENT.with(|current| {
            if let Some(state) = current.borrow_mut().as_mut() {
                state.stderr.push_str(text);
            }
        });
    }

    pub(super) fn take_stderr() -> String {
        CURRENT.with(|current| {
            current
                .borrow_mut()
                .as_mut()
                .map(|state| std::mem::take(&mut state.stderr))
                .unwrap_or_default()
        })
    }
}
