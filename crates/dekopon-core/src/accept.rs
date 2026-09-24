use std::io;

pub const ACCEPT_BACKOFF_MS: u64 = 100;

pub const MAX_ACCEPT_BACKOFF_MS: u64 = 1_000;

/// Restarting is expensive here: every provider recompiles under Cranelift before the socket
/// rebinds, taking minutes, so only truly fatal errors should stop the daemon.
#[cfg(unix)]
pub fn retryable_accept_error(error: &io::Error) -> Option<&'static str> {
    match error.raw_os_error()? {
        libc::EMFILE => Some("process-descriptor-limit"),
        libc::ENFILE => Some("system-descriptor-limit"),
        libc::ENOBUFS | libc::ENOMEM => Some("kernel-memory"),
        libc::ECONNABORTED => Some("connection-aborted"),
        libc::ECONNRESET => Some("connection-reset"),
        libc::EINTR => Some("interrupted"),
        _ => None,
    }
}

#[cfg(not(unix))]
pub fn retryable_accept_error(_error: &io::Error) -> Option<&'static str> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use std::io;

    use super::retryable_accept_error;

    #[test]
    fn transient_accept_failures_are_survivable_and_the_rest_are_not() {
        for (errno, kind) in [
            (libc::EMFILE, "process-descriptor-limit"),
            (libc::ENFILE, "system-descriptor-limit"),
            (libc::ENOBUFS, "kernel-memory"),
            (libc::ENOMEM, "kernel-memory"),
            (libc::ECONNABORTED, "connection-aborted"),
            (libc::ECONNRESET, "connection-reset"),
            (libc::EINTR, "interrupted"),
        ] {
            assert_eq!(
                retryable_accept_error(&io::Error::from_raw_os_error(errno)),
                Some(kind),
                "errno {errno} must not exit the daemon"
            );
        }

        for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK, libc::EOPNOTSUPP] {
            assert_eq!(
                retryable_accept_error(&io::Error::from_raw_os_error(errno)),
                None,
                "errno {errno} must stay fatal"
            );
        }
        assert_eq!(retryable_accept_error(&io::Error::other("no errno")), None);
    }
}
