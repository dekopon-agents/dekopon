use std::time::Duration;

use bytes::Bytes;
use dekopon_model_token_governor::{Admission, Outcome};
use futures_util::{Stream, StreamExt as _};
use serde_json::Value;
use tokio::time::Instant;

use crate::{
    CallSpan,
    dialect::{Dialect, Observation},
};

/// A single SSE line longer than this is forwarded but not parsed for usage.
const MAX_LINE_BYTES: usize = 1024 * 1024;
/// A non-streamed JSON answer longer than this is forwarded but its usage is estimated.
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const PING: &[u8] = b": ping\n\n";

#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    pub ping: Duration,
    pub idle: Duration,
}

pub(crate) enum Shape {
    Events,
    Json,
}

struct Tee<S> {
    upstream: S,
    dialect: Dialect,
    shape: Shape,
    timing: Timing,
    admission: Option<Admission>,
    span: CallSpan,
    observed: Observation,
    line: Vec<u8>,
    overlong: bool,
    at_boundary: bool,
    json: Vec<u8>,
    last_upstream: Instant,
    last_written: Instant,
    done: bool,
}

/// Forwards the upstream body unbuffered, feeds each event's usage and text into the admission,
/// and settles it when the upstream ends; a dropped body (the client went away) settles as
/// cancelled.
pub(crate) fn tee<S>(
    upstream: S,
    dialect: Dialect,
    shape: Shape,
    timing: Timing,
    admission: Option<Admission>,
    span: CallSpan,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    let now = Instant::now();
    let state = Tee {
        upstream,
        dialect,
        shape,
        timing,
        admission,
        span,
        observed: Observation::default(),
        line: Vec::new(),
        overlong: false,
        at_boundary: true,
        json: Vec::new(),
        last_upstream: now,
        last_written: now,
        done: false,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        let item = state.next().await?;
        Some((item, state))
    })
}

impl<S> Tee<S> {
    fn settle(&mut self, outcome: Outcome) {
        self.done = true;
        self.flush();
        let admission = self.admission.take();
        self.span.inner.in_scope(|| {
            if let Some(admission) = admission {
                admission.settle(outcome);
            }
        });
        self.span.settle(outcome);
    }

    /// Hands what was observed to the admission as it arrives, so a client that disconnects
    /// mid-stream is still charged for it when the admission drops.
    fn flush(&mut self) {
        if let Some(usage) = self.observed.usage.take() {
            self.span.observe(usage);
            if let Some(admission) = self.admission.as_ref() {
                admission.observe_usage(usage);
            }
        }
        if self.observed.text_bytes > 0 {
            let bytes = std::mem::take(&mut self.observed.text_bytes);
            if let Some(admission) = self.admission.as_ref() {
                admission.observe_text(bytes);
            }
        }
    }
}

impl<S> Tee<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    async fn next(&mut self) -> Option<Result<Bytes, std::io::Error>> {
        if self.done {
            return None;
        }
        loop {
            let pinging = matches!(self.shape, Shape::Events);
            let ping_at = self.last_written + self.timing.ping;
            let idle_at = self.last_upstream + self.timing.idle;
            let wake = if pinging {
                ping_at.min(idle_at)
            } else {
                idle_at
            };
            tokio::select! {
                chunk = self.upstream.next() => match chunk {
                    Some(Ok(chunk)) => {
                        let now = Instant::now();
                        self.last_upstream = now;
                        self.last_written = now;
                        self.read(&chunk);
                        return Some(Ok(chunk));
                    }
                    Some(Err(error)) => {
                        tracing::warn!(target: "model", event = "proxy.upstream_failed", error.kind = "stream", error = %error);
                        self.settle(Outcome::Failed);
                        return Some(Err(std::io::Error::other("upstream stream failed")));
                    }
                    None => {
                        self.finish_json();
                        self.settle(Outcome::Succeeded);
                        return None;
                    }
                },
                () = tokio::time::sleep_until(wake) => {
                    let now = Instant::now();
                    if now >= idle_at {
                        tracing::warn!(target: "model", event = "proxy.upstream_failed", error.kind = "idle");
                        self.settle(Outcome::Failed);
                        return Some(Err(std::io::Error::other("upstream went idle")));
                    }
                    if pinging && self.at_boundary {
                        self.last_written = now;
                        return Some(Ok(Bytes::from_static(PING)));
                    }
                    self.last_written = now;
                }
            }
        }
    }

    fn read(&mut self, chunk: &[u8]) {
        match self.shape {
            Shape::Json => {
                if self.json.len() + chunk.len() <= MAX_JSON_BYTES {
                    self.json.extend_from_slice(chunk);
                } else {
                    self.json = Vec::new();
                    self.shape_overflowed();
                }
            }
            Shape::Events => {
                for line in chunk.split_inclusive(|byte| *byte == b'\n') {
                    let complete = line.ends_with(b"\n");
                    if !self.overlong {
                        if self.line.len() + line.len() > MAX_LINE_BYTES {
                            self.overlong = true;
                            self.line.clear();
                        } else {
                            self.line.extend_from_slice(line);
                        }
                    }
                    if complete {
                        let blank = self.line.trim_ascii().is_empty() && !self.overlong;
                        if !self.overlong {
                            self.event_line();
                        }
                        self.line.clear();
                        self.overlong = false;
                        self.at_boundary = blank;
                    } else {
                        self.at_boundary = false;
                    }
                }
            }
        }
    }

    fn shape_overflowed(&mut self) {
        self.shape = Shape::Events;
        self.overlong = true;
        self.at_boundary = false;
    }

    fn event_line(&mut self) {
        let line = self.line.trim_ascii();
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let data = data.trim_ascii();
        if data == b"[DONE]" {
            return;
        }
        match serde_json::from_slice::<Value>(data) {
            Ok(event) => {
                self.dialect.observe(&event, &mut self.observed);
                self.flush();
            }
            Err(error) => {
                tracing::debug!(target: "model", error = %error, "proxied event was not JSON");
            }
        }
    }

    fn finish_json(&mut self) {
        if let Shape::Json = self.shape
            && !self.json.is_empty()
        {
            match serde_json::from_slice::<Value>(&self.json) {
                Ok(body) => self.dialect.observe_body(&body, &mut self.observed),
                Err(error) => {
                    tracing::debug!(target: "model", error = %error, "proxied answer was not JSON");
                }
            }
        }
    }
}
