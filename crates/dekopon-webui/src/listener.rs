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
    accept_backoff_ms: u64,
}

impl BoundedListener {
    pub(crate) fn new(listener: TcpListener, limits: WebUiLimits) -> Self {
        Self {
            listener,
            permits: Arc::new(Semaphore::new(limits.max_connections)),
            connection_timeout: limits.connection_timeout,
            accept_backoff_ms: ACCEPT_BACKOFF_MS,
        }
    }
}

impl axum::serve::Listener for BoundedListener {
    type Io = BoundedStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, address) = match self.listener.accept().await {
                Ok(accepted) => {
                    self.accept_backoff_ms = ACCEPT_BACKOFF_MS;
                    accepted
                }
                Err(error) => {
                    // One peer that vanished between its connect and this accept says nothing
                    // about the listener, so retrying immediately is correct and a wait would be
                    // a stall. It still names its cause once.
                    if is_connection_error(&error) {
                        tracing::debug!(
                            event = "webui_accept_failed",
                            category = "accept",
                            error.kind = "connection",
                            error = %error_chain(&error),
                        );
                        continue;
                    }
                    // A descriptor-table or memory failure recurs on the very next call, so a
                    // bare `continue` would spin this task at full CPU inside the broker. Unlike
                    // the broker's own accept loop this one cannot abort — `axum`'s `Listener`
                    // trait has no error path — so an unrecoverable errno waits at the ceiling
                    // and is named on every attempt rather than retried in silence.
                    let kind = retryable_accept_error(&error);
                    let backoff_ms = if kind.is_some() {
                        self.accept_backoff_ms
                    } else {
                        MAX_ACCEPT_BACKOFF_MS
                    };
                    tracing::warn!(
                        event = "webui_accept_failed",
                        category = "accept",
                        error.kind = kind.unwrap_or("unrecoverable"),
                        backoff_ms,
                        error = %error_chain(&error),
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    self.accept_backoff_ms = self
                        .accept_backoff_ms
                        .saturating_mul(2)
                        .min(MAX_ACCEPT_BACKOFF_MS);
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
