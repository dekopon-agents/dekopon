//! What a failed `accept()` says about the listening socket, and how long to wait.
//!
//! Two listeners in the broker process — the Unix control socket and the operational web UI's TCP
//! socket — must agree on which accept failures a socket recovers from. While the table lived
//! beside one of them the other retried forever at a flat one second on `EBADF` and reported no
//! errno at all, so the classification and the two waits live here instead of in either loop.

use std::io;

/// First wait after a retryable `accept()` failure.
///
/// Short enough that a descriptor freed by a closing connection is picked up almost immediately.
pub const ACCEPT_BACKOFF_MS: u64 = 100;

/// Ceiling the wait doubles up to while the condition persists.
pub const MAX_ACCEPT_BACKOFF_MS: u64 = 1_000;

/// Stable, low-cardinality name for an `accept` failure the loop can survive, or `None` if fatal.
///
/// Exiting is the expensive answer for the caller that can: that is the privileged daemon, so the
/// process ends, the container restarts, every provider recompiles under Cranelift before the
/// socket rebinds, and durable audit state waits through all of it — minutes, against a
/// five-minute startup probe. Descriptor exhaustion (which the unauthenticated `--http-bind`
/// listener can cause on its own), kernel buffer exhaustion, a client that vanished between its
/// connect and this accept, and a signal interruption are all conditions the next accept can
/// succeed through. None of them say the listener is broken, and none are worth a cold start.
///
/// The name is a telemetry value rather than a message: a loop that keeps retrying has to say
/// *which* exhaustion it is waiting on before an operator can act on it.
#[cfg(unix)]
pub fn retryable_accept_error(error: &io::Error) -> Option<&'static str> {
    match error.raw_os_error()? {
        libc::EMFILE => Some("process-descriptor-limit"),
        libc::ENFILE => Some("system-descriptor-limit"),
        // `accept` reports the same kernel-memory pressure under either name depending on the
        // platform and the allocation that failed.
        libc::ENOBUFS | libc::ENOMEM => Some("kernel-memory"),
        libc::ECONNABORTED => Some("connection-aborted"),
        libc::ECONNRESET => Some("connection-reset"),
        libc::EINTR => Some("interrupted"),
        _ => None,
    }
}

/// Unix `errno` values are the whole table, so nothing is classified as retryable elsewhere.
///
/// A caller that cannot abort keeps waiting at its ceiling, which is what it did before this
/// classification existed.
#[cfg(not(unix))]
pub fn retryable_accept_error(_error: &io::Error) -> Option<&'static str> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use std::io;

    use super::retryable_accept_error;

    /// One transient `accept` failure used to end the privileged daemon, and ending it is the most
    /// expensive answer available: the container restarts, every provider recompiles under
    /// Cranelift before the socket rebinds, and durable audit state waits through all of it.
    /// Descriptor exhaustion — which the unauthenticated `--http-bind` listener can cause on its
    /// own — is not a broken listener.
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

        // A listener that is gone, unbound, or not a socket is a real fault: retrying it forever
        // would turn a startup mistake into a silent hang.
        for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK, libc::EOPNOTSUPP] {
            assert_eq!(
                retryable_accept_error(&io::Error::from_raw_os_error(errno)),
                None,
                "errno {errno} must stay fatal"
            );
        }
        // Not every `io::Error` carries an errno.
        assert_eq!(retryable_accept_error(&io::Error::other("no errno")), None);
    }
}
