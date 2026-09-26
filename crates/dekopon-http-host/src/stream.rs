use super::asset::{AssetDirectory, AssetFile, AssetIoError, AssetReader, Completed};
use super::{
    BufferedHttpClient, ErrorCode, Header, HttpError, Request, Response, http_error,
    is_forbidden_response_header, map_reqwest_error,
};
use bytes::Bytes;
use dekopon_core::base64::{self, Engine as _};
use futures_util::StreamExt as _;
use http_body::{Body, Frame, SizeHint};
use sha2::{Digest as _, Sha256};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read as _},
    os::unix::fs::FileExt as _,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::time::timeout;
use tracing::Instrument as _;

pub const CHUNK_BYTES: usize = dekopon_core::asset::MAX_ASSET_CHUNK_BYTES;
const RAW_ENCODE_CHUNK: usize = CHUNK_BYTES / 4 * 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Representation {
    Identity,
    Base64,
}

pub struct FilePart {
    pub file: AssetReader,
    pub stored: Representation,
    pub wire: Representation,
    pub decoded_bytes: u64,
    pub id: Option<u64>,
    pub content_type: String,
}

pub enum Part {
    Literal(Vec<u8>),
    Asset(FilePart),
}

pub struct StreamedRequest {
    pub method: String,
    pub uri: String,
    pub headers: Vec<Header>,
    pub body: Vec<Part>,
}

pub struct StreamedResponse {
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: AssetFile,
}

#[derive(Clone, Copy)]
pub(super) struct RequestLengths {
    pub literal: u64,
    pub decoded_assets: u64,
    pub total: u64,
}

struct ActiveFile {
    part: FilePart,
    cursor: u64,
    digest: Sha256,
}
struct Chunk {
    active: ActiveFile,
    bytes: Bytes,
}
struct StreamBody {
    span: tracing::Span,
    parts: VecDeque<Part>,
    active: Option<ActiveFile>,
    pending: Option<tokio::task::JoinHandle<Completed<io::Result<Chunk>>>>,
    remaining: u64,
}

impl StreamBody {
    fn new(parts: Vec<Part>) -> Result<(Self, RequestLengths), HttpError> {
        let mut lengths = RequestLengths {
            literal: 0,
            decoded_assets: 0,
            total: 0,
        };
        for part in &parts {
            let bytes = match part {
                Part::Literal(bytes) => {
                    lengths.literal =
                        lengths
                            .literal
                            .checked_add(bytes.len() as u64)
                            .ok_or_else(|| {
                                http_error(ErrorCode::RequestTooLarge, "literal length overflow")
                            })?;
                    bytes.len() as u64
                }
                Part::Asset(asset) => {
                    lengths.decoded_assets = lengths
                        .decoded_assets
                        .checked_add(asset.decoded_bytes)
                        .filter(|bytes| {
                            *bytes <= dekopon_core::asset::MAX_DECODED_INVOCATION_BYTES as u64
                        })
                        .ok_or_else(|| {
                            http_error(
                                ErrorCode::RequestTooLarge,
                                "asset parts exceed the decoded invocation byte ceiling",
                            )
                        })?;
                    match asset.wire {
                        Representation::Identity => asset.decoded_bytes,
                        Representation::Base64 => base64::encoded_len(asset.decoded_bytes)
                            .map_err(|_overflow| {
                                http_error(ErrorCode::RequestTooLarge, "encoded length overflow")
                            })?,
                    }
                }
            };
            lengths.total = lengths
                .total
                .checked_add(bytes)
                .ok_or_else(|| http_error(ErrorCode::RequestTooLarge, "body length overflow"))?;
        }
        Ok((
            Self {
                span: tracing::Span::current(),
                parts: parts.into(),
                active: None,
                pending: None,
                remaining: lengths.total,
            },
            lengths,
        ))
    }
}

impl Body for StreamBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = self.get_mut();
        loop {
            if let Some(pending) = &mut this.pending {
                let result = ready!(Pin::new(pending).poll(cx)).map(|completed| completed.value);
                this.pending = None;
                let chunk = match result {
                    Ok(Ok(chunk)) => chunk,
                    Ok(Err(error)) => return Poll::Ready(Some(Err(error))),
                    Err(error) => return Poll::Ready(Some(Err(io::Error::other(error)))),
                };
                this.remaining -= chunk.bytes.len() as u64;
                if chunk.active.cursor == chunk.active.part.decoded_bytes {
                    this.span.in_scope(|| record_asset(chunk.active));
                } else {
                    this.active = Some(chunk.active);
                }
                return Poll::Ready(Some(Ok(Frame::data(chunk.bytes))));
            }
            if let Some(active) = this.active.take() {
                if active.cursor == active.part.decoded_bytes {
                    this.span.in_scope(|| record_asset(active));
                } else {
                    let span = this.span.clone();
                    let jobs = active.part.file.jobs().clone();
                    this.pending = Some(jobs.spawn(move || span.in_scope(|| read_chunk(active))));
                    continue;
                }
            }
            match this.parts.pop_front() {
                Some(Part::Literal(bytes)) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    this.remaining -= bytes.len() as u64;
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(bytes)))));
                }
                Some(Part::Asset(part)) => {
                    this.active = Some(ActiveFile {
                        part,
                        cursor: 0,
                        digest: Sha256::new(),
                    })
                }
                None => return Poll::Ready(None),
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }
}

fn record_asset(active: ActiveFile) {
    tracing::info!("asset.id" = active.part.id, "asset.content_type" = active.part.content_type,
        "asset.bytes" = active.cursor,
        "asset.sha256" = %active.digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
        "streamed asset consumed by HTTP body");
}

fn read_chunk(mut active: ActiveFile) -> io::Result<Chunk> {
    let count = (active.part.decoded_bytes - active.cursor).min(RAW_ENCODE_CHUNK as u64) as usize;
    let decoded = read_decoded(
        active.part.file.file(),
        active.part.stored,
        active.cursor,
        count,
    )?;
    active.digest.update(&decoded);
    active.cursor += decoded.len() as u64;
    let bytes = match active.part.wire {
        Representation::Identity => decoded,
        Representation::Base64 => {
            let started = std::time::Instant::now();
            let span = tracing::info_span!(
                "asset.encode",
                bytes = decoded.len(),
                duration_us = tracing::field::Empty
            );
            let encoded = span.in_scope(|| base64::STANDARD.encode(&decoded).into_bytes());
            span.record("duration_us", started.elapsed().as_micros() as u64);
            encoded
        }
    };
    Ok(Chunk {
        active,
        bytes: Bytes::from(bytes),
    })
}

struct PositionalReader<'a> {
    file: &'a File,
    offset: u64,
}
impl io::Read for PositionalReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.file.read_at(bytes, self.offset)?;
        self.offset += count as u64;
        Ok(count)
    }
}

pub fn read_decoded(
    file: &File,
    representation: Representation,
    offset: u64,
    count: usize,
) -> io::Result<Vec<u8>> {
    if count > CHUNK_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "asset read exceeds chunk bound",
        ));
    }
    let mut bytes = vec![0; count];
    match representation {
        Representation::Identity => file.read_exact_at(&mut bytes, offset)?,
        Representation::Base64 => {
            let started = std::time::Instant::now();
            let span = tracing::info_span!(
                "asset.decode",
                bytes = count,
                duration_us = tracing::field::Empty
            );
            span.in_scope(|| {
                let aligned = base64::read_offset(offset)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
                let reader = PositionalReader {
                    file,
                    offset: aligned.encoded,
                };
                let mut decoder = base64::DecoderReader::new(reader, &base64::STANDARD);
                let mut skip = [0; 2];
                decoder.read_exact(&mut skip[..usize::from(aligned.skip)])?;
                decoder.read_exact(&mut bytes)
            })?;
            span.record("duration_us", started.elapsed().as_micros() as u64);
        }
    }
    Ok(bytes)
}

impl BufferedHttpClient {
    /// stream() must apply exactly the same grant and credential checks as send(), or the two
    /// request paths could diverge in what they allow.
    pub async fn stream(
        &mut self,
        request: StreamedRequest,
        directory: &AssetDirectory,
    ) -> Result<StreamedResponse, HttpError> {
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
            .stream_checked(request, directory)
            .instrument(span.clone())
            .await;
        self.record_request(&span, index, &result);
        result
    }

    async fn stream_checked(
        &mut self,
        request: StreamedRequest,
        directory: &AssetDirectory,
    ) -> Result<StreamedResponse, HttpError> {
        let (body, lengths) = StreamBody::new(request.body)?;
        let (prepared, grant, index) = self
            .authorize_request(
                Request {
                    method: request.method,
                    uri: request.uri,
                    headers: request.headers,
                    body: Vec::new(),
                },
                Some(lengths),
            )
            .await?;
        let mut spool = directory
            .allocate()
            .await
            .map_err(|error| self.asset_error(error))?;
        let remaining = self.remaining()?;
        let client = self.pinned_client(&prepared.host, &prepared.addresses, remaining)?;
        let response = timeout(
            remaining,
            client
                .request(prepared.method, prepared.url)
                .headers(prepared.headers)
                .header(reqwest::header::CONTENT_LENGTH, lengths.total)
                .body(reqwest::Body::wrap(body))
                .send(),
        )
        .await
        .map_err(|_elapsed| http_error(ErrorCode::Timeout, "HTTP request timed out"))?
        .map_err(|error| map_reqwest_error(&error))?;
        if response
            .headers()
            .contains_key(reqwest::header::CONTENT_ENCODING)
        {
            return Err(http_error(
                ErrorCode::Protocol,
                "encoded streamed HTTP responses are refused",
            ));
        }
        let status = response.status().as_u16();
        self.evidence[index].status = Some(status);
        if response.headers().len() > self.ceilings.max_headers {
            return Err(http_error(
                ErrorCode::ResponseTooLarge,
                "response has too many headers",
            ));
        }
        let mut headers = Vec::with_capacity(response.headers().len());
        let mut response_bytes = 16_u64;
        for (name, value) in response.headers() {
            response_bytes = response_bytes
                .checked_add((name.as_str().len() + value.as_bytes().len() + 4) as u64)
                .ok_or_else(|| http_error(ErrorCode::ResponseTooLarge, "response size overflow"))?;
            if response_bytes > self.ceilings.max_header_bytes as u64
                || response_bytes > grant.max_response_bytes
            {
                return Err(http_error(
                    ErrorCode::ResponseTooLarge,
                    "response headers exceed an authorized bound",
                ));
            }
            if !is_forbidden_response_header(name) {
                headers.push(Header {
                    name: name.as_str().to_owned(),
                    value: value.as_bytes().to_vec(),
                });
            }
        }
        if self.credential.as_ref().is_some_and(|credential| {
            credential.echoes_credential(&Response {
                status,
                headers: headers.clone(),
                body: vec![],
            })
        }) {
            return Err(http_error(
                ErrorCode::Denied,
                "credentialed response echoed the credential",
            ));
        }
        let overlap = self.credential.as_ref().map_or(0, |credential| {
            credential
                .echo_values
                .iter()
                .map(|secret| secret.expose().len().saturating_sub(1))
                .max()
                .unwrap_or(0)
        });
        let mut window = Vec::with_capacity(CHUNK_BYTES + overlap);
        let mut response = response.bytes_stream();
        while let Some(chunk) = timeout(self.remaining()?, response.next())
            .await
            .map_err(|_elapsed| http_error(ErrorCode::Timeout, "response body timed out"))?
        {
            let chunk = chunk.map_err(|error| map_reqwest_error(&error))?;
            response_bytes = response_bytes
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| http_error(ErrorCode::ResponseTooLarge, "response size overflow"))?;
            self.evidence[index].response_bytes = response_bytes;
            if response_bytes > grant.max_response_bytes {
                return Err(http_error(
                    ErrorCode::ResponseTooLarge,
                    "response exceeds the authorized byte limit",
                ));
            }
            for bytes in chunk.chunks(CHUNK_BYTES) {
                if let Some(credential) = &self.credential
                    && credential.echoes_stream_chunk(&mut window, bytes, overlap)
                {
                    return Err(http_error(
                        ErrorCode::Denied,
                        "credentialed response echoed the credential",
                    ));
                }
                spool = spool
                    .write(bytes.to_vec())
                    .await
                    .map_err(|error| self.asset_error(error))?;
            }
        }
        self.evidence[index].status = Some(status);
        self.evidence[index].response_bytes = response_bytes;
        Ok(StreamedResponse {
            status,
            headers,
            body: spool
                .finish()
                .await
                .map_err(|error| self.asset_error(error))?,
        })
    }
}

impl BufferedHttpClient {
    fn asset_error(&mut self, error: AssetIoError) -> HttpError {
        if matches!(error, AssetIoError::OverBudget) {
            self.asset_over_budget = true;
        }
        match error {
            AssetIoError::OverBudget => http_error(
                ErrorCode::Internal,
                "over-budget: broker asset disk capacity exhausted",
            ),
            AssetIoError::Io { kind } => {
                tracing::error!(?kind, "streamed HTTP spool I/O failed");
                http_error(ErrorCode::Internal, "broker asset spool I/O failed")
            }
            AssetIoError::Worker => {
                http_error(ErrorCode::Internal, "broker asset spool worker failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_limit_matches_the_shared_contract() {
        assert_eq!(CHUNK_BYTES, dekopon_core::asset::MAX_ASSET_CHUNK_BYTES);
    }

    use crate::{BoundCredential, HttpHostCeilings};
    use dekopon_capability::HttpConstraints;
    use dekopon_core::Redacted;
    use dekopon_test_support::{LoopbackServer, content_length};
    use std::time::Duration;

    fn client(
        authority: &str,
        maximum: u64,
        credential: Option<BoundCredential>,
    ) -> BufferedHttpClient {
        BufferedHttpClient::authorized_with_credential(
            HttpConstraints {
                allowed_hosts: vec![authority.to_owned()],
                allowed_methods: vec!["POST".to_owned()],
                max_requests: 1,
                max_request_bytes: crate::encoded_request_bytes(
                    "POST",
                    &format!("http://{authority}/"),
                    0,
                    2,
                )
                .unwrap(),
                max_response_bytes: maximum,
                allow_plaintext_loopback: true,
                propagate_trace: false,
            },
            credential,
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .unwrap()
    }

    fn request(server: &LoopbackServer, body: Vec<Part>) -> StreamedRequest {
        StreamedRequest {
            method: "POST".to_owned(),
            uri: server.url(),
            headers: vec![],
            body,
        }
    }

    #[tokio::test]
    async fn exact_length_streaming_encodes_assets_without_charging_them_as_literals() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("input");
        let input = vec![b'x'; CHUNK_BYTES + 7];
        std::fs::write(&path, &input).unwrap();
        let server = LoopbackServer::once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let mut client = client(server.authority(), 1024, None);
        let file = FilePart {
            file: AssetReader::input(File::open(path).unwrap(), Default::default()),
            stored: Representation::Identity,
            wire: Representation::Base64,
            decoded_bytes: input.len() as u64,
            id: Some(1),
            content_type: "text/plain".to_owned(),
        };
        let response = client
            .stream(
                request(
                    &server,
                    vec![
                        Part::Literal(b"[".to_vec()),
                        Part::Asset(file),
                        Part::Literal(b"]".to_vec()),
                    ],
                ),
                &AssetDirectory::new(root.path().to_owned(), 2),
            )
            .await
            .unwrap();
        assert_eq!(
            read_decoded(response.body.file(), Representation::Identity, 0, 2).unwrap(),
            b"ok"
        );
        let wire = server.request();
        let start = wire
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        let expected = format!("[{}]", base64::STANDARD.encode(&input));
        assert_eq!(&wire[start..], expected.as_bytes());
        assert_eq!(content_length(&wire[..start]), expected.len());
        assert!(!String::from_utf8_lossy(&wire[..start]).contains("transfer-encoding"));
        assert_eq!(
            client.into_evidence()[0].request_bytes,
            expected.len() as u64
        );
        assert!(server.recorded().is_empty());
        server.join();
    }

    #[tokio::test]
    async fn literal_limit_accepts_the_edge_and_refuses_one_more_without_dispatch() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 1);
        let server = LoopbackServer::once(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        let mut too_large = client(server.authority(), 1024, None);
        assert_eq!(
            too_large
                .stream(
                    request(&server, vec![Part::Literal(b"123".to_vec())]),
                    &directory
                )
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::RequestTooLarge
        );
        let mut exact = client(server.authority(), 1024, None);
        exact
            .stream(
                request(&server, vec![Part::Literal(b"12".to_vec())]),
                &directory,
            )
            .await
            .unwrap();
        assert!(server.request_text().ends_with("12"));
        assert!(server.recorded().is_empty());
        server.join();
    }

    #[tokio::test]
    async fn streamed_headers_accept_the_grant_edge_and_refuse_one_more_without_dispatch() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 1);
        let server = LoopbackServer::once(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        let request = |value: &[u8]| StreamedRequest {
            headers: vec![Header {
                name: "x-note".to_owned(),
                value: value.to_vec(),
            }],
            ..request(&server, vec![Part::Literal(b"12".to_vec())])
        };
        let client = || {
            let mut client = client(server.authority(), 1024, None);
            client.grant.as_mut().unwrap().max_request_bytes += ("x-note".len() + 1 + 4) as u64;
            client
        };
        assert_eq!(
            client()
                .stream(request(b"aa"), &directory)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::RequestTooLarge
        );
        client().stream(request(b"a"), &directory).await.unwrap();
        let wire = server.request_text();
        assert!(wire.contains("x-note: a\r\n"));
        assert!(wire.ends_with("12"));
        assert!(server.recorded().is_empty());
        server.join();
    }

    #[tokio::test]
    async fn asset_limit_accepts_the_decoded_edge_and_refuses_one_more_without_dispatch() {
        use dekopon_core::asset::{MAX_DECODED_ASSET_BYTES, MAX_DECODED_INVOCATION_BYTES};
        let root = tempfile::tempdir().unwrap();
        let input = tempfile::NamedTempFile::new().unwrap();
        input
            .as_file()
            .set_len(MAX_DECODED_ASSET_BYTES as u64)
            .unwrap();
        for wire in [Representation::Identity, Representation::Base64] {
            let part = |bytes| {
                Part::Asset(FilePart {
                    file: AssetReader::input(File::open(input.path()).unwrap(), Default::default()),
                    stored: Representation::Identity,
                    wire,
                    decoded_bytes: bytes,
                    id: Some(1),
                    content_type: "application/octet-stream".to_owned(),
                })
            };
            let body = || {
                (0..5)
                    .map(|_| part(MAX_DECODED_ASSET_BYTES as u64))
                    .collect::<Vec<_>>()
            };
            let directory = AssetDirectory::new(root.path().to_owned(), 1);
            let server =
                LoopbackServer::once(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
            let mut too_large = client(server.authority(), 1024, None);
            let mut oversized = body();
            oversized.push(part(1));
            assert_eq!(
                too_large
                    .stream(request(&server, oversized), &directory)
                    .await
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::RequestTooLarge
            );
            let mut exact = client(server.authority(), 1024, None);
            exact
                .stream(request(&server, body()), &directory)
                .await
                .unwrap();
            let received = server.request();
            let start = received
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
                .unwrap()
                + 4;
            let expected = match wire {
                Representation::Identity => MAX_DECODED_INVOCATION_BYTES,
                Representation::Base64 => {
                    5 * base64::encoded_len(MAX_DECODED_ASSET_BYTES as u64).unwrap() as usize
                }
            };
            assert_eq!(content_length(&received[..start]), expected);
            assert_eq!(received.len() - start, expected);
            assert!(server.recorded().is_empty());
            server.join();
        }
    }

    #[tokio::test]
    async fn visible_header_names_and_values_are_scanned_with_clean_bodies_but_dropped_headers_are_not()
     {
        let secret = "secret-with-sixteen-bytes";
        for (header, denied) in [
            (format!("{secret}: harmless"), true),
            (format!("X-Echo: {secret}"), true),
            (format!("Set-Cookie: {secret}"), false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n{header}\r\n\r\nok"
            );
            let server = LoopbackServer::once(response.as_bytes());
            let credential = BoundCredential::bearer(
                "Bearer",
                Redacted::new(secret.to_owned()),
                vec![server.authority().to_owned()],
            )
            .unwrap();
            let mut client = client(server.authority(), 1024, Some(credential));
            let result = client
                .stream(
                    request(&server, vec![]),
                    &AssetDirectory::new(root.path().to_owned(), 1024),
                )
                .await;
            if denied {
                assert_eq!(result.err().unwrap().code, ErrorCode::Denied);
                assert!(client.policy_violation().is_some());
            } else {
                let response = result.unwrap();
                assert!(
                    !response
                        .headers
                        .iter()
                        .any(|header| header.name == "set-cookie")
                );
            }
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
            server.join();
        }
    }

    #[test]
    fn credential_echo_is_detected_at_every_deterministic_scanner_split() {
        let secret = "secret-with-sixteen-bytes";
        let credential = BoundCredential::bearer(
            "Bearer",
            Redacted::new(secret.to_owned()),
            vec!["example.test".to_owned()],
        )
        .unwrap();
        for split in 1..secret.len() {
            let mut window = Vec::new();
            assert!(!credential.echoes_stream_chunk(
                &mut window,
                &secret.as_bytes()[..split],
                secret.len() - 1
            ));
            assert!(credential.echoes_stream_chunk(
                &mut window,
                &secret.as_bytes()[split..],
                secret.len() - 1
            ));
        }
    }

    #[tokio::test]
    async fn encoded_responses_and_exhausted_spool_budgets_fail_without_leaving_files() {
        for encoding in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let response = if encoding {
                b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".as_slice()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".as_slice()
            };
            let server = LoopbackServer::once(response);
            let mut client = client(server.authority(), 1024, None);
            let error = client
                .stream(
                    request(&server, vec![]),
                    &AssetDirectory::new(root.path().to_owned(), 1),
                )
                .await
                .err()
                .unwrap();
            assert_eq!(
                error.code,
                if encoding {
                    ErrorCode::Protocol
                } else {
                    ErrorCode::Internal
                }
            );
            assert_eq!(client.asset_over_budget(), !encoding);
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
            server.join();
        }
    }

    #[test]
    fn cancelled_http_body_reads_hold_the_file_and_join_the_terminal_drain() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let directory = AssetDirectory::new(root.path().to_owned(), 4);
            let output = directory
                .allocate()
                .await
                .unwrap()
                .write(b"four".to_vec())
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let spare = directory.allocate().await.unwrap();
            let part = FilePart {
                file: AssetReader::output(std::sync::Arc::new(output)),
                stored: Representation::Identity,
                wire: Representation::Identity,
                decoded_bytes: 4,
                id: None,
                content_type: "text/plain".to_owned(),
            };
            let (mut body, _) = StreamBody::new(vec![Part::Asset(part)]).unwrap();
            let (started, running) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let gate = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            });
            running.await.unwrap();
            std::future::poll_fn(|cx| {
                assert!(Pin::new(&mut body).poll_frame(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(body);
            assert!(matches!(
                spare.write(vec![0]).await,
                Err(AssetIoError::OverBudget)
            ));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(25), directory.drain())
                    .await
                    .is_err()
            );
            release.send(()).unwrap();
            gate.await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), directory.drain())
                .await
                .unwrap();
            directory
                .allocate()
                .await
                .unwrap()
                .write(vec![0; 4])
                .await
                .unwrap();
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        });
    }

    #[test]
    fn decoded_reads_are_bounded_and_short_positional_reads_abort() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(CHUNK_BYTES as u64).unwrap();
        assert_eq!(
            read_decoded(&file, Representation::Identity, 0, CHUNK_BYTES)
                .unwrap()
                .len(),
            CHUNK_BYTES
        );
        assert_eq!(
            read_decoded(&file, Representation::Identity, 0, CHUNK_BYTES + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            read_decoded(&file, Representation::Identity, 1, CHUNK_BYTES)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn wrapped_exact_body_is_not_cloneable_and_reports_its_exact_size() {
        let (body, lengths) = StreamBody::new(vec![Part::Literal(b"abc".to_vec())]).unwrap();
        assert_eq!(body.size_hint().exact(), Some(3));
        assert_eq!(lengths.total, 3);
        assert!(
            reqwest::Client::new()
                .post("http://127.0.0.1/")
                .body(reqwest::Body::wrap(body))
                .build()
                .unwrap()
                .try_clone()
                .is_none()
        );
    }
    #[tokio::test]
    async fn encoded_chunk_edge_is_one_frame_and_one_more_byte_requires_a_second() {
        for count in [RAW_ENCODE_CHUNK, RAW_ENCODE_CHUNK + 1] {
            let file = tempfile::tempfile().unwrap();
            file.set_len(count as u64).unwrap();
            let part = FilePart {
                file: AssetReader::input(file, Default::default()),
                stored: Representation::Identity,
                wire: Representation::Base64,
                decoded_bytes: count as u64,
                id: Some(1),
                content_type: "application/octet-stream".to_owned(),
            };
            let (mut body, _) = StreamBody::new(vec![Part::Asset(part)]).unwrap();
            let mut frames = 0;
            while let Some(frame) =
                std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
            {
                let data = frame.unwrap().into_data().unwrap();
                assert!(data.len() <= CHUNK_BYTES);
                frames += 1;
            }
            assert_eq!(frames, if count == RAW_ENCODE_CHUNK { 1 } else { 2 });
            assert_eq!(body.size_hint().exact(), Some(0));
        }
    }
    #[tokio::test]
    async fn streamed_response_accounting_accepts_the_exact_grant_and_refuses_one_byte_over() {
        for maximum in [56, 55] {
            let root = tempfile::tempdir().unwrap();
            let server = LoopbackServer::once(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            );
            let mut client = client(server.authority(), maximum, None);
            let result = client
                .stream(
                    request(&server, vec![]),
                    &AssetDirectory::new(root.path().to_owned(), 2),
                )
                .await;
            if maximum == 56 {
                assert!(result.is_ok());
                assert_eq!(client.into_evidence()[0].response_bytes, 56);
            } else {
                assert_eq!(result.err().unwrap().code, ErrorCode::ResponseTooLarge);
                assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
            }
            server.join();
        }
    }
}
