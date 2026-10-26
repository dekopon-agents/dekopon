use super::asset::{AssetDirectory, AssetFile, AssetIoError, AssetReader, Completed};
use super::{
    BoundCredential, BufferedHttpClient, ErrorCode, Header, HttpError, Request, Response,
    http_error, is_forbidden_response_header, map_reqwest_error,
};
use bytes::{Buf as _, Bytes};
use dekopon_capability::HttpConstraints;
use dekopon_core::base64::{self, Engine as _};
use flate2::bufread::{GzDecoder, ZlibDecoder};
use http_body::{Body, Frame, SizeHint};
use reqwest::{Method, StatusCode};
use sha2::{Digest as _, Sha256};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, BufRead, Read},
    os::unix::fs::FileExt as _,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::time::{Instant, timeout, timeout_at};
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
        "http.asset.streamed");
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
                .request(prepared.method.clone(), prepared.url)
                .headers(prepared.headers)
                .header(reqwest::header::CONTENT_LENGTH, lengths.total)
                .body(reqwest::Body::wrap(body))
                .send(),
        )
        .await
        .map_err(|_elapsed| http_error(ErrorCode::Timeout, "HTTP request timed out"))?
        .map_err(|error| map_reqwest_error(&error))?;
        let Head {
            status,
            headers,
            bytes: mut response_bytes,
            mut body,
        } = self.accept_head(
            &prepared.method,
            response,
            &grant,
            index,
            super::ResponseMetadata::Ordinary,
        )?;
        let overlap = self
            .credential
            .as_ref()
            .map_or(0, BoundCredential::echo_overlap);
        let mut window = Vec::with_capacity(CHUNK_BYTES + overlap);
        while let Some(chunk) = body.next().await? {
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
                if let Some(credential) = &self.credential {
                    let clean = credential
                        .scan_stream_chunk(&mut window, bytes, overlap)
                        .ok_or_else(echoed)?;
                    window.drain(..clean);
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

pub(crate) fn echoed() -> HttpError {
    http_error(
        ErrorCode::Denied,
        "credentialed response echoed the credential",
    )
}

#[derive(Default)]
struct Pending {
    chunk: Bytes,
    ended: bool,
}

impl Read for Pending {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.fill_buf()?.len().min(buffer.len());
        buffer[..count].copy_from_slice(&self.chunk[..count]);
        self.consume(count);
        Ok(count)
    }
}

impl BufRead for Pending {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.chunk.is_empty() && !self.ended {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(&self.chunk)
    }

    fn consume(&mut self, amount: usize) {
        self.chunk.advance(amount);
    }
}

enum Decoder {
    Gzip(GzDecoder<Pending>),
    Deflate(ZlibDecoder<Pending>),
}

impl Decoder {
    fn read(&mut self, decoded: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Gzip(decoder) => decoder.read(decoded),
            Self::Deflate(decoder) => decoder.read(decoded),
        }
    }

    fn pending(&mut self) -> &mut Pending {
        match self {
            Self::Gzip(decoder) => decoder.get_mut(),
            Self::Deflate(decoder) => decoder.get_mut(),
        }
    }
}

pub(crate) struct Head {
    pub(crate) status: u16,
    pub(crate) headers: Vec<Header>,
    pub(crate) bytes: u64,
    pub(crate) body: ResponseBody,
}

pub(crate) struct ResponseBody {
    response: reqwest::Response,
    decoder: Option<Decoder>,
    deadline: Instant,
}

impl ResponseBody {
    pub(crate) async fn next(&mut self) -> Result<Option<Bytes>, HttpError> {
        let Some(decoder) = &mut self.decoder else {
            return next_chunk(&mut self.response, self.deadline).await;
        };
        let mut decoded = vec![0; CHUNK_BYTES];
        loop {
            match decoder.read(&mut decoded) {
                Ok(0) => {
                    let pending = decoder.pending();
                    let mut trailing = pending.chunk.len();
                    let mut ended = pending.ended;
                    // HTTP/2 servers commonly close the stream with an empty DATA frame.
                    while trailing == 0 && !ended {
                        match next_chunk(&mut self.response, self.deadline).await? {
                            Some(chunk) => trailing = chunk.len(),
                            None => ended = true,
                        }
                    }
                    if trailing > 0 {
                        return Err(http_error(
                            ErrorCode::Protocol,
                            "response carries bytes after its encoded body",
                        ));
                    }
                    return Ok(None);
                }
                Ok(count) => {
                    decoded.truncate(count);
                    return Ok(Some(decoded.into()));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    match next_chunk(&mut self.response, self.deadline).await? {
                        Some(chunk) => decoder.pending().chunk = chunk,
                        None => decoder.pending().ended = true,
                    }
                }
                Err(_error) => {
                    return Err(http_error(
                        ErrorCode::Protocol,
                        "encoded response body is corrupt",
                    ));
                }
            }
        }
    }
}

async fn next_chunk(
    response: &mut reqwest::Response,
    deadline: Instant,
) -> Result<Option<Bytes>, HttpError> {
    timeout_at(deadline, response.chunk())
        .await
        .map_err(|_elapsed| http_error(ErrorCode::Timeout, "response body timed out"))?
        .map_err(|error| map_reqwest_error(&error))
}

impl BufferedHttpClient {
    pub(crate) fn accept_head(
        &mut self,
        method: &Method,
        response: reqwest::Response,
        grant: &HttpConstraints,
        index: usize,
        metadata: super::ResponseMetadata,
    ) -> Result<Head, HttpError> {
        let mut codings = response
            .headers()
            .get_all(reqwest::header::CONTENT_ENCODING)
            .iter();
        let bodyless = *method == Method::HEAD
            || response.status() == StatusCode::NO_CONTENT
            || response.status() == StatusCode::NOT_MODIFIED
            || response.content_length() == Some(0);
        let decoder = match (
            codings.next().map(reqwest::header::HeaderValue::as_bytes),
            codings.next(),
        ) {
            (None, _) => None,
            (Some(_), _) if bodyless => None,
            (Some(coding), None) if coding.eq_ignore_ascii_case(b"gzip") => {
                Some(Decoder::Gzip(GzDecoder::new(Pending::default())))
            }
            (Some(coding), None) if coding.eq_ignore_ascii_case(b"deflate") => {
                Some(Decoder::Deflate(ZlibDecoder::new(Pending::default())))
            }
            // A body the host does not decode would reach the credential echo scan still encoded.
            (Some(_), _) => {
                return Err(http_error(
                    ErrorCode::Protocol,
                    "response content encoding is not one the host decodes",
                ));
            }
        };
        let status = response.status().as_u16();
        self.evidence[index].status = Some(status);
        let response_bytes = super::response_head_bytes(
            response.headers(),
            &self.ceilings,
            grant.max_response_bytes,
        )?;
        let mut headers = Vec::with_capacity(response.headers().len());
        for (name, value) in response.headers() {
            if !is_forbidden_response_header(name)
                && (decoder.is_none()
                    || (name != reqwest::header::CONTENT_ENCODING
                        && name != reqwest::header::CONTENT_LENGTH))
            {
                headers.push(Header {
                    name: name.as_str().to_owned(),
                    value: value.as_bytes().to_vec(),
                });
            }
        }
        let head = Response {
            status,
            headers,
            body: vec![],
        };
        if self
            .credential
            .as_ref()
            .is_some_and(|credential| credential.echoes_credential(&head))
        {
            return Err(echoed());
        }
        self.evidence[index].response_bytes = response_bytes;
        self.evidence[index].upstream_request_id = match (metadata, status) {
            (super::ResponseMetadata::SignedDownload, 302) => None,
            (super::ResponseMetadata::Ordinary, _)
            | (super::ResponseMetadata::SignedDownload, _) => response
                .headers()
                .get("x-github-request-id")
                .and_then(|value| {
                    let bytes = value.as_bytes();
                    (1..=64)
                        .contains(&bytes.len())
                        .then_some(bytes)
                        .filter(|bytes| bytes.iter().all(|byte| (0x21..=0x7e).contains(byte)))
                        .and_then(|bytes| std::str::from_utf8(bytes).ok())
                        .map(str::to_owned)
                }),
        };
        Ok(Head {
            status,
            headers: head.headers,
            bytes: response_bytes,
            body: ResponseBody {
                response,
                decoder,
                deadline: self.deadline,
            },
        })
    }

    pub(super) fn asset_error(&mut self, error: AssetIoError) -> HttpError {
        if matches!(error, AssetIoError::OverBudget) {
            self.asset_over_budget = true;
        }
        match error {
            AssetIoError::OverBudget => http_error(
                ErrorCode::Internal,
                "over-budget: broker asset disk capacity exhausted",
            ),
            AssetIoError::Io { kind } => {
                tracing::error!(?kind, "http.spool.failed");
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
    use flate2::{
        Compression,
        write::{GzEncoder, ZlibEncoder},
    };
    use std::{io::Write as _, time::Duration};

    fn client(
        authority: &str,
        maximum: u64,
        credential: Option<BoundCredential>,
    ) -> BufferedHttpClient {
        BufferedHttpClient::authorized_with_credential(
            HttpConstraints {
                allowed_hosts: vec![authority.to_owned()],
                allowed_methods: vec!["POST".to_owned(), "HEAD".to_owned()],
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
                request_templates: Vec::new(),
                signed_download: None,
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
            assert!(
                credential
                    .scan_stream_chunk(&mut window, &secret.as_bytes()[..split], secret.len() - 1)
                    .is_some()
            );
            assert!(
                credential
                    .scan_stream_chunk(&mut window, &secret.as_bytes()[split..], secret.len() - 1)
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn encoded_responses_and_exhausted_spool_budgets_fail_without_leaving_files() {
        for encoding in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let response = if encoding {
                b"HTTP/1.1 200 OK\r\nContent-Encoding: br\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".as_slice()
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

    #[derive(Clone, Copy)]
    enum Path {
        Send,
        Stream,
    }

    async fn fetch(
        path: Path,
        method: &str,
        response: &[u8],
        maximum: u64,
        secret: Option<&str>,
    ) -> Result<(Vec<Header>, Vec<u8>), ErrorCode> {
        let root = tempfile::tempdir().unwrap();
        let server = LoopbackServer::once(response);
        let credential = secret.map(|secret| {
            BoundCredential::bearer(
                "Bearer",
                Redacted::new(secret.to_owned()),
                vec![server.authority().to_owned()],
            )
            .unwrap()
        });
        let mut client = client(server.authority(), maximum, credential);
        let result = match path {
            Path::Send => client
                .send(Request {
                    method: method.to_owned(),
                    uri: server.url(),
                    headers: vec![],
                    body: vec![],
                })
                .await
                .map(|response| (response.headers, response.body)),
            Path::Stream => client
                .stream(
                    StreamedRequest {
                        method: method.to_owned(),
                        ..request(&server, vec![])
                    },
                    &AssetDirectory::new(root.path().to_owned(), maximum),
                )
                .await
                .map(|response| {
                    let body = read_decoded(
                        response.body.file(),
                        Representation::Identity,
                        0,
                        usize::try_from(response.body.len()).unwrap(),
                    )
                    .unwrap();
                    (response.headers, body)
                }),
        };
        assert!(
            server
                .request_text()
                .to_ascii_lowercase()
                .contains("accept-encoding: gzip, deflate\r\n")
        );
        server.join();
        result.map_err(|error| error.code)
    }

    fn encoded(encoding: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: {encoding}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn gzip(plain: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(plain).unwrap();
        encoder.finish().unwrap()
    }

    fn zlib(plain: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(plain).unwrap();
        encoder.finish().unwrap()
    }

    #[tokio::test]
    async fn send_and_stream_share_the_pre_dns_request_decision() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let grant = HttpConstraints {
            allowed_hosts: vec!["no-such-egress.invalid".to_owned()],
            allowed_methods: vec!["POST".to_owned()],
            max_requests: 2,
            max_request_bytes: 4096,
            max_response_bytes: 1024,
            allow_plaintext_loopback: false,
            propagate_trace: false,
            request_templates: Vec::new(),
            signed_download: None,
        };
        let mut client = BufferedHttpClient::authorized(
            grant,
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .unwrap()
        .with_request_authorizer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { false })
        }));
        let uri = "https://no-such-egress.invalid/reply".to_owned();
        let denied = client
            .send(Request {
                method: "POST".to_owned(),
                uri: uri.clone(),
                headers: vec![],
                body: vec![],
            })
            .await
            .expect_err("send refuses before DNS");
        assert_eq!(denied.code, ErrorCode::Denied);
        let root = tempfile::tempdir().unwrap();
        let denied = client
            .stream(
                StreamedRequest {
                    method: "POST".to_owned(),
                    uri,
                    headers: vec![],
                    body: vec![],
                },
                &AssetDirectory::new(root.path().to_owned(), 1024),
            )
            .await;
        assert!(matches!(
            denied,
            Err(HttpError {
                code: ErrorCode::Denied,
                ..
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn every_delivered_body_is_decoded_or_refused_on_send_and_stream() {
        let plain = b"plain response text";
        for path in [Path::Send, Path::Stream] {
            for (response, expected) in [
                (encoded("gzip", &gzip(plain)), Ok(plain.to_vec())),
                (encoded("deflate", &zlib(plain)), Ok(plain.to_vec())),
                (encoded("br", plain), Err(ErrorCode::Protocol)),
                (
                    encoded("gzip\r\nContent-Encoding: gzip", &gzip(&gzip(plain))),
                    Err(ErrorCode::Protocol),
                ),
                (
                    encoded("gzip", &[gzip(plain), b"trailing".to_vec()].concat()),
                    Err(ErrorCode::Protocol),
                ),
            ] {
                let result = fetch(path, "POST", &response, 1024, None).await;
                if let Ok((headers, _)) = &result {
                    assert!(
                        headers
                            .iter()
                            .all(|header| header.name != "content-encoding")
                    );
                }
                assert_eq!(result.map(|(_, body)| body), expected);
            }
        }
    }

    #[tokio::test]
    async fn a_credential_echoed_inside_a_compressed_body_is_refused_on_send_and_stream() {
        let secret = "secret-with-sixteen-bytes";
        let response = encoded(
            "gzip",
            &gzip(format!("{}Bearer {secret}", "echo ".repeat(64)).as_bytes()),
        );
        assert!(
            !response
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
        );
        for path in [Path::Send, Path::Stream] {
            assert_eq!(
                fetch(path, "POST", &response, 1024, Some(secret))
                    .await
                    .err(),
                Some(ErrorCode::Denied)
            );
        }
    }

    #[tokio::test]
    async fn the_response_limit_counts_decoded_bytes_on_send_and_stream() {
        let maximum = 64 * 1024;
        let bomb = gzip(&vec![0; 1024 * 1024]);
        assert!((bomb.len() as u64) < maximum / 8);
        let response = encoded("gzip", &bomb);
        for path in [Path::Send, Path::Stream] {
            assert_eq!(
                fetch(path, "POST", &response, maximum, None).await.err(),
                Some(ErrorCode::ResponseTooLarge)
            );
        }
    }

    #[tokio::test]
    async fn a_bodyless_response_with_a_content_encoding_needs_no_decoding_on_send_and_stream() {
        for path in [Path::Send, Path::Stream] {
            for (method, response) in [
                (
                    "HEAD",
                    b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 1234\r\nConnection: close\r\n\r\n".as_slice(),
                ),
                (
                    "POST",
                    b"HTTP/1.1 204 No Content\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\n".as_slice(),
                ),
                (
                    "POST",
                    b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(),
                ),
            ] {
                let (_, body) = fetch(path, method, response, 1024, None).await.unwrap();
                assert!(body.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn an_empty_frame_after_the_encoded_body_is_not_trailing_data_on_send_and_stream() {
        let plain = b"plain response text";
        let certified =
            rcgen::generate_simple_self_signed(vec!["fixture.example.test".to_owned()]).unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certified.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der())
                .into(),
        )
        .unwrap();
        config.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let authority = format!("fixture.example.test:{}", address.port());
        let encoded = Bytes::from(gzip(plain));
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let tls = acceptor.accept(stream).await.unwrap();
                let encoded = encoded.clone();
                tokio::spawn(async move {
                    let mut connection = h2::server::handshake(tls).await.unwrap();
                    let (_request, mut respond) = connection.accept().await.unwrap().unwrap();
                    let response = http::Response::builder()
                        .header("content-encoding", "gzip")
                        .body(())
                        .unwrap();
                    let mut send = respond.send_response(response, false).unwrap();
                    send.send_data(encoded, false).unwrap();
                    send.send_data(Bytes::new(), true).unwrap();
                    while let Some(Ok(_)) = connection.accept().await {}
                });
            }
        });
        for path in [Path::Send, Path::Stream] {
            let root = tempfile::tempdir().unwrap();
            let pin = crate::LoopbackHttpsPin::new(
                &authority,
                address,
                certified.cert.pem().into_bytes(),
            )
            .unwrap();
            let mut client = BufferedHttpClient::authorized(
                HttpConstraints {
                    allowed_hosts: vec![authority.clone()],
                    allowed_methods: vec!["GET".to_owned()],
                    max_requests: 1,
                    max_request_bytes: 1024,
                    max_response_bytes: 1024,
                    allow_plaintext_loopback: false,
                    propagate_trace: false,
                    request_templates: Vec::new(),
                    signed_download: None,
                },
                HttpHostCeilings {
                    loopback_https_pin: Some(pin),
                    ..HttpHostCeilings::default()
                },
                Duration::from_secs(5),
            )
            .unwrap();
            let uri = format!("https://{authority}/");
            let body = match path {
                Path::Send => {
                    client
                        .send(Request {
                            method: "GET".to_owned(),
                            uri,
                            headers: vec![],
                            body: vec![],
                        })
                        .await
                        .unwrap()
                        .body
                }
                Path::Stream => {
                    let response = client
                        .stream(
                            StreamedRequest {
                                method: "GET".to_owned(),
                                uri,
                                headers: vec![],
                                body: vec![],
                            },
                            &AssetDirectory::new(root.path().to_owned(), 1024),
                        )
                        .await
                        .unwrap();
                    read_decoded(
                        response.body.file(),
                        Representation::Identity,
                        0,
                        usize::try_from(response.body.len()).unwrap(),
                    )
                    .unwrap()
                }
            };
            assert_eq!(body, plain);
        }
        server.await.unwrap();
    }
}
