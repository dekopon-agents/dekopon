use std::fmt;

use crate::provider::Stdin;

#[cfg(target_arch = "wasm32")]
mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "spawn-client",
        with: {
            "dekopon:stdio/streams@0.1.0": crate::export_bindings::dekopon::stdio::streams,
        },
        generate_all,
    });
}

/// What a child script reads as its stdin.
pub enum ChildStdin {
    /// Nothing piped into the child.
    None,
    /// This invocation's own stdin, shared as a Unix child shares a descriptor.
    Inherit,
    /// Bytes the host pumps from this reader into the child.
    Reader(Stdin),
}

/// Why a child script was not started.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnError {
    /// A child is still running: wait on it first.
    Busy,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Busy => "a child script is still running; wait on it first",
        })
    }
}

impl std::error::Error for SpawnError {}

/// A child's exit status and the bounded stderr the gateway kept.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Exit {
    /// The child script's exit status.
    pub status: u8,
    /// The bounded standard error the gateway kept.
    pub stderr: String,
}

/// A running child script: its stdout, and its exit through [`Child::wait`].
pub struct Child {
    /// The child's standard output; an empty read is end of output.
    pub stdout: Stdin,
    status: Status,
}

#[cfg(target_arch = "wasm32")]
use bindings::dekopon::spawn::run::Status;
#[cfg(not(target_arch = "wasm32"))]
enum Status {}

impl Child {
    /// Waits for the child to exit; stdout not yet read is discarded first.
    #[must_use]
    pub fn wait(self) -> Exit {
        let Self { stdout, status } = self;
        drop(stdout);
        #[cfg(target_arch = "wasm32")]
        {
            let exit = Status::wait(status);
            Exit {
                status: exit.status,
                stderr: exit.stderr,
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        match status {}
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn run(script: &str, stdin: ChildStdin) -> Result<Child, SpawnError> {
    use bindings::dekopon::spawn::run as wit;
    let stdin = match stdin {
        ChildStdin::None => wit::Stdin::None,
        ChildStdin::Inherit => wit::Stdin::Inherit,
        ChildStdin::Reader(reader) => wit::Stdin::Reader(reader.into_guest()),
    };
    match wit::run(script, stdin) {
        Ok(child) => Ok(Child {
            stdout: Stdin::from_guest(child.stdout),
            status: child.status,
        }),
        Err(wit::SpawnError::Busy) => Err(SpawnError::Busy),
    }
}
