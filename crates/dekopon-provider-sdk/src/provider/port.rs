use std::cell::RefCell;
use std::io;

pub trait Port {
    fn now_unix_millis(&mut self) -> u64;
    fn now_nanos(&mut self) -> u64;
    fn fill_random(&mut self, out: &mut [u8]);
    fn settings(&mut self) -> Option<String>;
    fn send(
        &mut self,
        request: crate::http::Request,
    ) -> Result<crate::http::Response, crate::http::HttpError>;
    fn stream(
        &mut self,
        request: crate::http::StreamedRequest<'_>,
    ) -> Result<crate::http::StreamedResponse, crate::http::HttpError>;
    #[cfg(feature = "native-test")]
    fn signed_download(
        &mut self,
        _request: &crate::signed_download::Request,
    ) -> Result<crate::signed_download::Response, crate::signed_download::DownloadError> {
        Err(crate::signed_download::DownloadError {
            code: crate::signed_download::ErrorCode::Denied,
        })
    }
    fn spawn(&mut self, script: &str, stdin: NativeChildStdin) -> NativeChild {
        drop(stdin);
        panic!("this native Port runs no child script: {script}");
    }
}

pub enum NativeChildStdin {
    None,
    Inherit(Box<dyn io::Read>),
    Reader(Box<dyn io::Read>),
}

pub struct NativeChild {
    pub stdout: Box<dyn io::Read>,
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

#[cfg(feature = "native-test")]
pub(crate) fn signed_download(
    request: &crate::signed_download::Request,
) -> Result<crate::signed_download::Response, crate::signed_download::DownloadError> {
    CURRENT.with(|current| match current.borrow_mut().as_mut() {
        Some(port) => port.signed_download(request),
        None => Err(crate::signed_download::DownloadError {
            code: crate::signed_download::ErrorCode::Denied,
        }),
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
    fn a_native_port_without_spawn_refuses_the_child() {
        let error = std::panic::catch_unwind(|| {
            with_port(Fake(1), || spawn("gh pr list", NativeChildStdin::None))
        })
        .err()
        .expect("default port refuses spawn");
        assert_eq!(
            error.downcast_ref::<String>().map(String::as_str),
            Some("this native Port runs no child script: gh pr list")
        );
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
