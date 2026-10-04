use std::cell::RefCell;
use std::io;

/// Native import implementation installed for one scoped provider invocation.
pub trait Port {
    /// Reads the broker wall clock.
    fn now_unix_millis(&mut self) -> u64;
    /// Reads elapsed nanoseconds on the native test port.
    fn now_nanos(&mut self) -> u64;
    /// Fills a bounded buffer from the native test port's entropy source.
    fn fill_random(&mut self, out: &mut [u8]);
    /// Reads provider settings under the invocation grant.
    fn settings(&mut self) -> Option<String>;
    /// Sends a buffered HTTP request.
    fn send(
        &mut self,
        request: crate::http::Request,
    ) -> Result<crate::http::Response, crate::http::HttpError>;
    /// Sends an asset-backed request.
    fn stream(
        &mut self,
        request: crate::http::StreamedRequest<'_>,
    ) -> Result<crate::http::StreamedResponse, crate::http::HttpError>;
    /// Runs a child script; a port that scripts no child refuses the call by panicking.
    fn spawn(&mut self, script: &str, stdin: NativeChildStdin) -> NativeChild {
        drop(stdin);
        panic!("this native Port runs no child script: {script}");
    }
}

/// What a native child script reads as its stdin.
pub enum NativeChildStdin {
    /// Nothing piped into the child.
    None,
    /// The rest of the invocation's own stdin.
    Inherit(Box<dyn io::Read>),
    /// A reader the capability passed.
    Reader(Box<dyn io::Read>),
}

/// A child script run by a native port: its output and how it exited.
pub struct NativeChild {
    /// The child's standard output.
    pub stdout: Box<dyn io::Read>,
    /// The child's exit.
    pub exit: crate::spawn::Exit,
}

thread_local! {
    static CURRENT: RefCell<Option<Box<dyn Port>>> = RefCell::new(None);
}

struct Restore(Option<Box<dyn Port>>);

impl Drop for Restore {
    fn drop(&mut self) {
        CURRENT.with(|current| {
            *current.borrow_mut() = self.0.take();
        });
    }
}

/// Installs a native port for the duration of `f`, restoring the previous port even on panic.
pub fn with_port<R>(port: impl Port + 'static, f: impl FnOnce() -> R) -> R {
    let previous = CURRENT.with(|current| current.replace(Some(Box::new(port))));
    let _restore = Restore(previous);
    f()
}

pub(crate) fn now_unix_millis() -> u64 {
    CURRENT.with(|current| {
        current
            .borrow_mut()
            .as_mut()
            .expect("native call requires an installed Port")
            .now_unix_millis()
    })
}

pub(crate) fn now_nanos() -> u64 {
    CURRENT.with(|current| {
        current
            .borrow_mut()
            .as_mut()
            .expect("native call requires an installed Port")
            .now_nanos()
    })
}

pub(crate) fn fill_random(out: &mut [u8]) {
    CURRENT.with(|current| {
        current
            .borrow_mut()
            .as_mut()
            .expect("native call requires an installed Port")
            .fill_random(out)
    })
}

pub(crate) fn settings() -> Option<String> {
    CURRENT.with(|current| {
        current
            .borrow_mut()
            .as_mut()
            .expect("native call requires an installed Port")
            .settings()
    })
}

pub(crate) fn send(
    request: crate::http::Request,
) -> Result<crate::http::Response, crate::http::HttpError> {
    CURRENT.with(|current| {
        current
            .borrow_mut()
            .as_mut()
            .expect("native call requires an installed Port")
            .send(request)
    })
}

pub(crate) fn stream(
    request: crate::http::StreamedRequest<'_>,
) -> Result<crate::http::StreamedResponse, crate::http::HttpError> {
    CURRENT.with(|current| {
        current
            .borrow_mut()
            .as_mut()
            .expect("native call requires an installed Port")
            .stream(request)
    })
}

pub(crate) fn spawn(script: &str, stdin: NativeChildStdin) -> NativeChild {
    CURRENT.with(|current| {
        current
            .borrow_mut()
            .as_mut()
            .expect("native call requires an installed Port")
            .spawn(script, stdin)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake(u64);
    impl Port for Fake {
        fn now_unix_millis(&mut self) -> u64 {
            self.0
        }
        fn now_nanos(&mut self) -> u64 {
            self.0
        }
        fn fill_random(&mut self, out: &mut [u8]) {
            out.fill(42);
        }
        fn settings(&mut self) -> Option<String> {
            Some(self.0.to_string())
        }
        fn send(
            &mut self,
            _: crate::http::Request,
        ) -> Result<crate::http::Response, crate::http::HttpError> {
            unreachable!()
        }
        fn stream(
            &mut self,
            _: crate::http::StreamedRequest<'_>,
        ) -> Result<crate::http::StreamedResponse, crate::http::HttpError> {
            unreachable!()
        }
    }
    #[test]
    fn nested_ports_restore_the_outer_value_even_on_panic() {
        with_port(Fake(1), || {
            assert_eq!(now_unix_millis(), 1);
            let caught = std::panic::catch_unwind(|| {
                with_port(Fake(2), || {
                    assert_eq!(now_unix_millis(), 2);
                    panic!("restore");
                })
            });
            assert!(caught.is_err());
            assert_eq!(now_unix_millis(), 1);
        });
        assert!(std::panic::catch_unwind(now_unix_millis).is_err());
    }
}
