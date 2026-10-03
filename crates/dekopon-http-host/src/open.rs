use std::time::Duration;

use bytes::Bytes;
use futures_util::{StreamExt as _, stream::BoxStream};
use tokio::time::{Instant, timeout};
use tracing::Instrument as _;

use super::stream::echoed;
use super::{
    BoundCredential, BufferedHttpClient, ErrorCode, Header, HttpError, Request, http_error,
    map_reqwest_error,
};

pub struct OpenedResponse {
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: ResponseBody,
}

/// Bytes leave only as scanned clean prefixes; the held tail goes out at a clean end of body and is
/// dropped by any failure.
pub struct ResponseBody {
    chunks: BoxStream<'static, reqwest::Result<Bytes>>,
    scan: Option<(BoundCredential, usize)>,
    window: Vec<u8>,
    released: Bytes,
    accounted: u64,
    limit: u64,
    // Only time waiting on the connection is charged, so a slow consumer never times a request out.
    budget: Duration,
    span: tracing::Span,
    state: State,
}

enum BodyOutcome<'a> {
    Succeeded,
    Failed(&'a HttpError),
    Abandoned,
}

enum State {
    Open,
    Ended,
    Failed(HttpError),
}

impl ResponseBody {
    /// Returns at most `max` clean bytes; an empty result is the clean end of body.
    pub async fn read(&mut self, max: usize) -> Result<Vec<u8>, HttpError> {
        while self.released.is_empty() {
            match &self.state {
                State::Open => {}
                State::Ended => return Ok(Vec::new()),
                State::Failed(error) => return Err(error.clone()),
            }
            if let Err(error) = self.pull().await {
                self.window = Vec::new();
                self.finish(BodyOutcome::Failed(&error));
                self.state = State::Failed(error);
            }
        }
        let count = self.released.len().min(max);
        Ok(self.released.split_to(count).to_vec())
    }

    async fn pull(&mut self) -> Result<(), HttpError> {
        let started = Instant::now();
        let next = timeout(self.budget, self.chunks.next()).await;
        self.budget = self.budget.saturating_sub(started.elapsed());
        let Some(chunk) =
            next.map_err(|_elapsed| http_error(ErrorCode::Timeout, "response body timed out"))?
        else {
            self.released = Bytes::from(std::mem::take(&mut self.window));
            self.finish(BodyOutcome::Succeeded);
            self.state = State::Ended;
            return Ok(());
        };
        let chunk = chunk.map_err(|error| map_reqwest_error(&error))?;
        self.accounted = self
            .accounted
            .checked_add(chunk.len() as u64)
            .filter(|accounted| *accounted <= self.limit)
            .ok_or_else(|| {
                http_error(
                    ErrorCode::ResponseTooLarge,
                    "response exceeds the authorized byte limit",
                )
            })?;
        let Some((credential, overlap)) = &self.scan else {
            self.released = chunk;
            return Ok(());
        };
        let clean = credential
            .scan_stream_chunk(&mut self.window, &chunk, *overlap)
            .ok_or_else(echoed)?;
        self.released = self.window.drain(..clean).collect::<Vec<_>>().into();
        Ok(())
    }

    fn finish(&self, outcome: BodyOutcome<'_>) {
        self.span
            .record("dekopon.http.response.accounted_bytes", self.accounted);
        self.span.in_scope(|| {
            let (label, failure) = match outcome {
                BodyOutcome::Succeeded => ("succeeded", None),
                BodyOutcome::Failed(error) => ("failed", Some(error)),
                BodyOutcome::Abandoned => ("abandoned", None),
            };
            tracing::info!(
                target: "dekopon_http_host::audit",
                {
                    audit.event = "accounting.http.response_body",
                    "dekopon.http.response.accounted_bytes" = self.accounted,
                    "error.code" = failure.map(|error| tracing::field::debug(error.code)),
                    "error.message" = failure.map(|error| error.message.as_str()),
                    outcome = label,
                },
                "opened HTTP response body ended"
            );
        });
    }
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        match self.state {
            State::Open => self.finish(BodyOutcome::Abandoned),
            State::Ended | State::Failed(_) => {}
        }
    }
}

impl BufferedHttpClient {
    /// open() applies exactly the grant and credential checks of send(); only the body differs.
    pub async fn open(&mut self, request: Request) -> Result<OpenedResponse, HttpError> {
        self.attempted = true;
        let span = tracing::info_span!(
            "http.request",
            "url.full" = request.uri,
            "http.request.method" = tracing::field::Empty,
            "server.address" = tracing::field::Empty,
            "http.response.status_code" = tracing::field::Empty,
            "dekopon.http.request.accounted_bytes" = tracing::field::Empty,
            "dekopon.http.response.accounted_bytes" = tracing::field::Empty,
            "error.code" = tracing::field::Empty,
            "error.message" = tracing::field::Empty,
            outcome = tracing::field::Empty
        );
        let index = self.evidence.len();
        let result = self
            .open_checked(request, span.clone())
            .instrument(span.clone())
            .await;
        self.record_request(&span, index, &result);
        result
    }

    async fn open_checked(
        &mut self,
        request: Request,
        span: tracing::Span,
    ) -> Result<OpenedResponse, HttpError> {
        let (prepared, grant, index) = self.authorize_request(request, None).await?;
        let remaining = self.remaining()?;
        let client = self.pinned_client(&prepared.host, &prepared.addresses, remaining)?;
        let response = timeout(
            remaining,
            client
                .request(prepared.method, prepared.url)
                .headers(prepared.headers)
                .body(prepared.body)
                .send(),
        )
        .await
        .map_err(|_elapsed| http_error(ErrorCode::Timeout, "HTTP request timed out"))?
        .map_err(|error| map_reqwest_error(&error))?;
        let (status, headers, accounted) = self.accept_head(&response, &grant, index)?;
        let scan = self.credential.clone().map(|credential| {
            let overlap = credential.echo_overlap();
            (credential, overlap)
        });
        Ok(OpenedResponse {
            status,
            headers,
            body: ResponseBody {
                chunks: response.bytes_stream().boxed(),
                scan,
                window: Vec::new(),
                released: Bytes::new(),
                accounted,
                limit: grant.max_response_bytes,
                budget: self.remaining()?,
                span,
                state: State::Open,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dekopon_capability::HttpConstraints;
    use dekopon_core::Redacted;
    use dekopon_test_support::LoopbackServer;

    use super::*;
    use crate::HttpHostCeilings;

    const SECRET: &[u8] = b"secret-with-sixteen-bytes";
    const HELD: usize = SECRET.len() - 1;

    fn head(length: usize) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n")
            .into_bytes()
    }

    fn client(server: &LoopbackServer, timeout: Duration) -> BufferedHttpClient {
        let credential = BoundCredential::bearer(
            "Bearer",
            Redacted::new(String::from_utf8(SECRET.to_vec()).unwrap()),
            vec![server.authority().to_owned()],
        )
        .unwrap();
        BufferedHttpClient::authorized_with_credential(
            HttpConstraints {
                allowed_hosts: vec![server.authority().to_owned()],
                allowed_methods: vec!["GET".to_owned()],
                max_requests: 1,
                max_request_bytes: 4096,
                max_response_bytes: 1 << 20,
                allow_plaintext_loopback: true,
                propagate_trace: false,
            },
            Some(credential),
            HttpHostCeilings::default(),
            timeout,
        )
        .unwrap()
    }

    fn get(server: &LoopbackServer) -> Request {
        Request {
            method: "GET".to_owned(),
            uri: server.url(),
            headers: vec![],
            body: vec![],
        }
    }

    async fn drain(body: &mut ResponseBody) -> (Vec<u8>, Result<(), HttpError>) {
        let mut delivered = Vec::new();
        loop {
            match body.read(usize::MAX).await {
                Ok(bytes) if bytes.is_empty() => return (delivered, Ok(())),
                Ok(bytes) => delivered.extend(bytes),
                Err(error) => return (delivered, Err(error)),
            }
        }
    }

    #[tokio::test]
    async fn a_late_echo_across_two_chunks_releases_only_the_clean_prefix_and_drops_the_tail() {
        let first = [vec![b'a'; 64], SECRET[..8].to_vec()].concat();
        let second = [SECRET[8..].to_vec(), b"tail".to_vec()].concat();
        let server = LoopbackServer::paced(
            vec![
                [head(first.len() + second.len()), first.clone()].concat(),
                second,
            ],
            Duration::from_millis(100),
            Duration::ZERO,
        );
        let mut client = client(&server, Duration::from_secs(5));
        let mut opened = client.open(get(&server)).await.unwrap();
        let (delivered, outcome) = drain(&mut opened.body).await;
        assert_eq!(outcome.unwrap_err().code, ErrorCode::Denied);
        assert_eq!(delivered, first[..first.len() - HELD]);
        assert!(delivered.iter().all(|byte| *byte == b'a'));
        assert_eq!(
            opened.body.read(usize::MAX).await.unwrap_err().code,
            ErrorCode::Denied
        );
        server.join();
    }

    #[tokio::test]
    async fn a_read_parked_on_a_stalled_body_times_out_and_drops_the_held_tail() {
        let first = [vec![b'a'; 64], SECRET[..8].to_vec()].concat();
        let server = LoopbackServer::paced(
            vec![[head(first.len() + 100), first.clone()].concat()],
            Duration::ZERO,
            Duration::from_secs(3),
        );
        let mut client = client(&server, Duration::from_millis(500));
        let mut opened = client.open(get(&server)).await.unwrap();
        let (delivered, outcome) = drain(&mut opened.body).await;
        assert_eq!(outcome.unwrap_err().code, ErrorCode::Timeout);
        assert_eq!(delivered, first[..first.len() - HELD]);
    }

    #[tokio::test]
    async fn dropping_an_unread_body_records_one_abandoned_outcome_under_its_request() {
        use dekopon_test_support::CaptureLayer;
        use tracing_subscriber::layer::SubscriberExt as _;

        let capture = CaptureLayer::workspace();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let _guard = tracing::subscriber::set_default(subscriber);
        let server = LoopbackServer::once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        );
        let mut client = client(&server, Duration::from_secs(5));
        drop(client.open(get(&server)).await.unwrap());
        let bodies = capture
            .events()
            .into_iter()
            .filter(|(fields, _)| fields.contains("accounting.http.response_body"))
            .collect::<Vec<_>>();
        assert_eq!(bodies.len(), 1, "{}", capture.text());
        assert!(bodies[0].0.contains("outcome=\"abandoned\""), "{bodies:?}");
        assert!(
            bodies[0]
                .0
                .contains("dekopon.http.response.accounted_bytes=54"),
            "{bodies:?}"
        );
        assert_eq!(bodies[0].1.as_deref(), Some("http.request"));
        server.join();
    }

    #[tokio::test]
    async fn time_spent_between_reads_is_not_charged_to_the_request_deadline() {
        let part = vec![b'b'; 32];
        let server = LoopbackServer::paced(
            vec![
                [head(3 * part.len()), part.clone()].concat(),
                part.clone(),
                part.clone(),
            ],
            Duration::from_millis(500),
            Duration::ZERO,
        );
        let mut client = client(&server, Duration::from_millis(400));
        let mut opened = client.open(get(&server)).await.unwrap();
        let mut delivered = opened.body.read(usize::MAX).await.unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        let (rest, outcome) = drain(&mut opened.body).await;
        outcome.unwrap();
        delivered.extend(rest);
        assert_eq!(delivered, vec![b'b'; 3 * part.len()]);
        server.join();
    }
}
