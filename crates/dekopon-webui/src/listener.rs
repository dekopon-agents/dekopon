//! Connection ceilings for the informational HTTP listener.
//!
//! The dashboard is an unauthenticated TCP surface inside the privileged broker process, whose
//! worst container failure is an OOM kill. `axum::serve` on a bare `TcpListener` spawns one task
//! per accepted connection with no cap and no deadline, so this module mirrors the broker's Unix
//! socket instead: a fixed number of concurrent connections, refused rather than queued when
//! saturated, each with one wall-clock budget covering header read, render, and body write.

use std::{
    future::Future as _,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use dekopon_core::{ACCEPT_BACKOFF_MS, MAX_ACCEPT_BACKOFF_MS, error_chain, retryable_accept_error};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Sleep,
};

use crate::WebUiLimits;

/// Wraps a bound listener so accepted connections are counted and time-bounded.
pub(crate) struct BoundedListener {
    listener: TcpListener,
    permits: Arc<Semaphore>,
    connection_timeout: Duration,
    backoff: AcceptBackoff,
}

impl BoundedListener {
    pub(crate) fn new(listener: TcpListener, limits: WebUiLimits) -> Self {
        Self {
            listener,
            permits: Arc::new(Semaphore::new(limits.max_connections)),
            connection_timeout: limits.connection_timeout,
            backoff: AcceptBackoff::new(),
        }
    }
}

/// What one failed `accept()` costs the loop, kept apart from the socket it happened on.
///
/// The three outcomes this decides — retry now, wait and retry, wait at the ceiling forever — are
/// exactly the ones a real `TcpListener` cannot be made to produce on demand: nothing a test can
/// do to a bound socket exhausts the descriptor table or closes the descriptor underneath it. The
/// schedule and the wording therefore live here, where they are reachable without one.
struct AcceptBackoff {
    next_ms: u64,
}

impl AcceptBackoff {
    const fn new() -> Self {
        Self {
            next_ms: ACCEPT_BACKOFF_MS,
        }
    }

    /// Returns the schedule to its first wait, which one healthy accept is proof it should.
    const fn reset(&mut self) {
        self.next_ms = ACCEPT_BACKOFF_MS;
    }

    /// Names one failed accept and answers how long to wait, or `None` to retry immediately.
    fn note(&mut self, error: &io::Error) -> Option<Duration> {
        // One peer that vanished between its connect and this accept says nothing about the
        // listener, so retrying immediately is correct and a wait would be a stall. It still names
        // its cause once, and it leaves the schedule alone: a dropped connection must not slow the
        // next real one down.
        if is_connection_error(error) {
            tracing::debug!(
                event = "webui_accept_failed",
                category = "accept",
                error.kind = "connection",
                error = %error_chain(error),
            );
            return None;
        }
        // A descriptor-table or memory failure recurs on the very next call, so a bare `continue`
        // would spin this task at full CPU inside the broker. Unlike the broker's own accept loop
        // this one cannot abort — `axum`'s `Listener` trait has no error path — so an unrecoverable
        // errno waits at the ceiling and is named on every attempt rather than retried in silence.
        let kind = retryable_accept_error(error);
        let backoff_ms = if kind.is_some() {
            self.next_ms
        } else {
            MAX_ACCEPT_BACKOFF_MS
        };
        tracing::warn!(
            event = "webui_accept_failed",
            category = "accept",
            error.kind = kind.unwrap_or("unrecoverable"),
            backoff_ms,
            error = %error_chain(error),
        );
        self.next_ms = self.next_ms.saturating_mul(2).min(MAX_ACCEPT_BACKOFF_MS);
        Some(Duration::from_millis(backoff_ms))
    }
}

impl axum::serve::Listener for BoundedListener {
    type Io = BoundedStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, address) = match self.listener.accept().await {
                Ok(accepted) => {
                    self.backoff.reset();
                    accepted
                }
                Err(error) => {
                    if let Some(wait) = self.backoff.note(&error) {
                        tokio::time::sleep(wait).await;
                    }
                    continue;
                }
            };
            // Refuse rather than queue: a waiting connection is retained memory and a retained
            // descriptor, which is exactly what the cap exists to bound.
            let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
                drop(stream);
                tracing::debug!(
                    event = "webui_connection_refused",
                    reason = "connection_limit"
                );
                continue;
            };
            return (
                BoundedStream::new(stream, permit, self.connection_timeout),
                address,
            );
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

fn is_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// One accepted connection holding its concurrency permit until it closes.
///
/// The deadline is absolute from accept, not per read or per write: a slow-reading client pins the
/// whole rendered response, so only a budget spanning the entire connection bounds it. Expiry
/// surfaces as an I/O error, which drops the connection and releases the permit.
pub(crate) struct BoundedStream {
    stream: TcpStream,
    deadline: Option<Pin<Box<Sleep>>>,
    expired: bool,
    _permit: OwnedSemaphorePermit,
}

impl BoundedStream {
    fn new(stream: TcpStream, permit: OwnedSemaphorePermit, timeout: Duration) -> Self {
        Self {
            stream,
            deadline: Some(Box::pin(tokio::time::sleep(timeout))),
            expired: false,
            _permit: permit,
        }
    }

    /// Registers the deadline waker so an idle connection is still cut when its budget elapses.
    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        if self.expired {
            return true;
        }
        let Some(deadline) = self.deadline.as_mut() else {
            return false;
        };
        if deadline.as_mut().poll(cx).is_pending() {
            return false;
        }
        self.expired = true;
        self.deadline = None;
        tracing::debug!(event = "webui_connection_expired", reason = "io_timeout");
        true
    }
}

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "Dekopon web UI connection exceeded its deadline",
    )
}

impl AsyncRead for BoundedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(timed_out()));
        }
        Pin::new(&mut this.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for BoundedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(timed_out()));
        }
        Pin::new(&mut this.stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(timed_out()));
        }
        Pin::new(&mut this.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(timed_out()));
        }
        Pin::new(&mut this.stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Shutdown is the connection ending anyway; a lapsed deadline must not keep it open.
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io;

    use axum::serve::Listener as _;
    use dekopon_core::{ACCEPT_BACKOFF_MS, MAX_ACCEPT_BACKOFF_MS};
    use dekopon_test_support::CaptureLayer;
    use tokio::net::{TcpListener, TcpStream};
    use tracing_subscriber::{layer::SubscriberExt as _, registry};

    use super::{AcceptBackoff, BoundedListener, Duration};
    use crate::WebUiLimits;

    /// Runs `body` against a capture, returning what it produced beside every event it emitted.
    fn captured<T>(body: impl FnOnce() -> T) -> (T, String) {
        let capture = CaptureLayer::workspace();
        let produced = tracing::subscriber::with_default(registry().with(capture.clone()), body);
        (produced, capture.events_text())
    }

    /// A peer that vanished between its connect and this accept says nothing about the listener.
    ///
    /// Waiting on it would stall every other pending connection behind a client that is already
    /// gone, and inflating the schedule would make the next genuine failure start further along
    /// it. The loop retries at once, names the cause once, and leaves the schedule untouched.
    #[test]
    fn a_vanished_peer_is_retried_immediately_and_named_once() {
        let mut backoff = AcceptBackoff::new();

        let (waits, events) = captured(|| {
            [libc::ECONNABORTED, libc::ECONNRESET]
                .map(|errno| backoff.note(&io::Error::from_raw_os_error(errno)))
        });

        assert_eq!(waits, [None, None], "a vanished peer must not be waited on");
        assert_eq!(
            events.matches("error.kind=\"connection\"").count(),
            2,
            "{events}"
        );
        assert!(!events.contains("WARN"), "{events}");
        assert_eq!(
            backoff.next_ms, ACCEPT_BACKOFF_MS,
            "a dropped connection must not slow the next real one down"
        );
    }

    /// A descriptor-table failure recurs on the very next call, so the loop backs off and says
    /// which exhaustion it is waiting on — every attempt, not only the first.
    #[test]
    fn a_retryable_failure_doubles_from_100ms_to_the_1s_ceiling() {
        let mut backoff = AcceptBackoff::new();

        let (waits, events) = captured(|| {
            (0..6)
                .map(|_| {
                    backoff
                        .note(&io::Error::from_raw_os_error(libc::EMFILE))
                        .expect("a retryable failure waits before retrying")
                        .as_millis()
                })
                .collect::<Vec<_>>()
        });

        assert_eq!(waits, vec![100, 200, 400, 800, 1_000, 1_000]);
        assert_eq!(
            events
                .matches("error.kind=\"process-descriptor-limit\"")
                .count(),
            6,
            "an operator cannot act on an exhaustion that is only named once: {events}"
        );
    }

    /// `EBADF` is not survivable, and this loop cannot abort: `axum`'s `Listener` has no error
    /// path. It waits at the ceiling instead of spinning, and every attempt carries the errno —
    /// the whole diagnosable content of a listener that will never accept again.
    #[test]
    fn an_unrecoverable_failure_waits_at_the_ceiling_and_names_its_errno() {
        let mut backoff = AcceptBackoff::new();

        let (wait, events) = captured(|| backoff.note(&io::Error::from_raw_os_error(libc::EBADF)));

        assert_eq!(wait, Some(Duration::from_millis(MAX_ACCEPT_BACKOFF_MS)));
        assert!(events.starts_with("WARN"), "{events}");
        assert!(events.contains("error.kind=\"unrecoverable\""), "{events}");
        assert!(events.contains("backoff_ms=1000"), "{events}");
        assert!(
            events.contains(&format!("os error {}", libc::EBADF)),
            "{events}"
        );
    }

    /// One healthy accept is proof the condition cleared, so the next failure starts at 100 ms
    /// rather than inheriting a second of waiting from a descriptor pressure that is over.
    #[tokio::test]
    async fn a_successful_accept_returns_the_schedule_to_its_first_wait() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("bound address");
        let mut bounded = BoundedListener::new(listener, WebUiLimits::default());
        // Set rather than provoked: what the schedule reached is this test's precondition, and
        // `a_retryable_failure_doubles_from_100ms_to_the_1s_ceiling` owns how it gets there.
        bounded.backoff.next_ms = MAX_ACCEPT_BACKOFF_MS;

        let client = TcpStream::connect(address).await.expect("client connects");
        let (accepted, peer) = bounded.accept().await;

        assert!(peer.ip().is_loopback(), "{peer}");
        assert_eq!(
            bounded.backoff.next_ms, ACCEPT_BACKOFF_MS,
            "a healthy accept must not inherit the previous failure's wait"
        );
        drop(accepted);
        drop(client);
    }
}
