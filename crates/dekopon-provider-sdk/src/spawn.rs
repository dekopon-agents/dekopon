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
struct Status {
    exit: Exit,
    _live: Live,
}

#[cfg(not(target_arch = "wasm32"))]
thread_local! {
    static LIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct Live(());

#[cfg(not(target_arch = "wasm32"))]
impl Live {
    pub(crate) fn claim() -> Option<Self> {
        LIVE.with(|live| (!live.replace(true)).then_some(Self(())))
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Live {
    fn drop(&mut self) {
        LIVE.with(|live| live.set(false));
    }
}

impl Child {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn native(child: crate::provider::NativeChild, live: Live) -> Self {
        Self {
            stdout: Stdin::from_native(child.stdout),
            status: Status {
                exit: child.exit,
                _live: live,
            },
        }
    }

    /// Waits for the child to exit; stdout not yet read is discarded first.
    ///
    /// ```compile_fail
    /// fn read_after_wait(child: dekopon_provider_sdk::provider::Child) {
    ///     let _exit = child.wait();
    ///     let _stdout = child.stdout;
    /// }
    /// ```
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
        {
            status.exit
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn native_child_slot_is_busy_until_wait_or_drop() {
        let first = Child::native(
            crate::provider::NativeChild {
                stdout: Box::new(std::io::empty()),
                exit: Exit {
                    status: 0,
                    stderr: String::new(),
                },
            },
            Live::claim().expect("first child"),
        );
        assert!(Live::claim().is_none());
        assert_eq!(first.wait().status, 0);
        let second = Child::native(
            crate::provider::NativeChild {
                stdout: Box::new(std::io::empty()),
                exit: Exit {
                    status: 0,
                    stderr: String::new(),
                },
            },
            Live::claim().expect("wait releases slot"),
        );
        assert!(Live::claim().is_none());
        drop(second);
        assert!(Live::claim().is_some());
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
