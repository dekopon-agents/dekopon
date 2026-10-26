use super::{
    BufferedHttpClient, ErrorCode, Header, HttpCallEvidence, HttpError, Request, Response,
};
use crate::asset::{AssetDirectory, AssetFile};
use dekopon_capability::{HttpConstraints, SignedDownload};
use reqwest::{Method, Url};
use tokio::time::timeout_at;
use tracing::Instrument as _;

const WINDOW_BYTES: u32 = 65_536;
const LOCATION_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Window {
    Tail(u32),
    Offset { start: u64, length: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Coverage {
    pub start: u64,
    pub end_exclusive: u64,
    pub total: u64,
}

pub enum DownloadResponse {
    Api(Response),
    Storage {
        status: u16,
        coverage: Option<Coverage>,
        body: Option<AssetFile>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DownloadError {
    #[error("signed download is not authorized")]
    Denied,
    #[error("signed download redirect refused")]
    RedirectRefused,
    #[error("signed download request limit exhausted")]
    RequestLimit,
    #[error("signed download timed out")]
    Timeout,
    #[error("signed download exceeds the response limit")]
    ResponseTooLarge,
    #[error("signed download is incomplete")]
    Incomplete,
    #[error("signed download storage request failed")]
    Storage,
    #[error("signed download asset I/O failed")]
    Internal,
}

impl DownloadError {
    fn label(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::RedirectRefused => "redirect-refused",
            Self::RequestLimit => "host-call-limit",
            Self::Timeout => "timeout",
            Self::ResponseTooLarge => "response-too-large",
            Self::Incomplete => "incomplete",
            Self::Storage => "storage",
            Self::Internal => "internal",
        }
    }
}

impl From<HttpError> for DownloadError {
    fn from(error: HttpError) -> Self {
        match error.code {
            ErrorCode::HostCallLimit => Self::RequestLimit,
            ErrorCode::Timeout => Self::Timeout,
            ErrorCode::ResponseTooLarge => Self::ResponseTooLarge,
            ErrorCode::Denied
            | ErrorCode::InvalidMethod
            | ErrorCode::InvalidUri
            | ErrorCode::InvalidHeader
            | ErrorCode::RequestTooLarge => Self::Denied,
            ErrorCode::Dns | ErrorCode::Connect | ErrorCode::Tls | ErrorCode::Protocol => {
                Self::Storage
            }
            ErrorCode::Internal => Self::Internal,
        }
    }
}

pub(super) struct Destination {
    uri: dekopon_core::Redacted<Url>,
    client: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    authority: String,
    request_bytes: u64,
    span: tracing::Span,
}

#[derive(Default)]
struct DownloadScan {
    location: Vec<u8>,
    credential: Vec<u8>,
}

enum FetchedRange {
    Range { coverage: Coverage, body: AssetFile },
    Status(u16),
}

impl Destination {
    async fn new(
        uri: Url,
        grant: HttpConstraints,
        ceilings: super::HttpHostCeilings,
        deadline: tokio::time::Instant,
    ) -> Result<Self, DownloadError> {
        let budget = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .ok_or(DownloadError::Timeout)?;
        let mut validator = BufferedHttpClient::authorized(grant.clone(), ceilings, budget)
            .map_err(|_cause| DownloadError::Internal)?;
        validator.deadline = deadline;
        let prepared = validator
            .prepare(
                Request {
                    method: "GET".to_owned(),
                    uri: uri.as_str().to_owned(),
                    headers: vec![
                        Header {
                            name: "accept-encoding".to_owned(),
                            value: b"identity".to_vec(),
                        },
                        Header {
                            name: "user-agent".to_owned(),
                            value: b"dekopon".to_vec(),
                        },
                    ],
                    body: vec![],
                },
                &grant,
                None,
            )
            .await
            .map_err(|error| match error.code {
                ErrorCode::Denied => DownloadError::RedirectRefused,
                ErrorCode::InvalidMethod
                | ErrorCode::InvalidUri
                | ErrorCode::InvalidHeader
                | ErrorCode::RequestTooLarge
                | ErrorCode::HostCallLimit
                | ErrorCode::Dns
                | ErrorCode::Connect
                | ErrorCode::Tls
                | ErrorCode::Timeout
                | ErrorCode::Protocol
                | ErrorCode::ResponseTooLarge
                | ErrorCode::Internal => DownloadError::from(error),
            })?;
        let client = validator
            .pinned_client(
                &prepared.host,
                &prepared.addresses,
                validator.remaining().map_err(DownloadError::from)?,
            )
            .map_err(DownloadError::from)?;
        Ok(Self {
            uri: dekopon_core::Redacted::new(prepared.url),
            client,
            headers: prepared.headers,
            authority: prepared.authority,
            request_bytes: prepared.request_bytes,
            span: tracing::Span::none(),
        })
    }
}

impl BufferedHttpClient {
    pub async fn signed_download(
        &mut self,
        api_uri: String,
        window: Window,
        directory: &AssetDirectory,
    ) -> Result<DownloadResponse, DownloadError> {
        let result = self
            .signed_download_checked(api_uri, window, directory)
            .await;
        let violation = match &result {
            Ok(_) => None,
            Err(error) => match error {
                DownloadError::Denied => Some(super::PolicyViolation::Denied),
                DownloadError::RedirectRefused => Some(super::PolicyViolation::RedirectRefused),
                DownloadError::RequestLimit => Some(super::PolicyViolation::HostCallLimit),
                DownloadError::ResponseTooLarge => Some(super::PolicyViolation::ByteLimit),
                DownloadError::Timeout
                | DownloadError::Incomplete
                | DownloadError::Storage
                | DownloadError::Internal => None,
            },
        };
        self.policy_violation = violation.or(self.policy_violation);
        result
    }

    async fn signed_download_checked(
        &mut self,
        api_uri: String,
        window: Window,
        directory: &AssetDirectory,
    ) -> Result<DownloadResponse, DownloadError> {
        self.attempted = true;
        if self.credential.is_none() {
            return Err(DownloadError::Denied);
        }
        let grant = self.grant.as_ref().ok_or(DownloadError::Denied)?;
        match grant.signed_download {
            Some(SignedDownload::GithubActionsLogsV1) => {}
            None => return Err(DownloadError::Denied),
        }
        let length = match window {
            Window::Tail(length) | Window::Offset { length, .. } => length,
        };
        if length == 0 || length > WINDOW_BYTES || !api_route(&api_uri) {
            return Err(DownloadError::Denied);
        }
        let api = self.signed_api_request(api_uri).await?;
        if api.status != 302 {
            return Ok(DownloadResponse::Api(api));
        }
        let uri = location(&api.headers)?;
        let mut grant = self.grant.clone().ok_or(DownloadError::Denied)?;
        grant.allowed_hosts = vec![
            uri.host_str()
                .ok_or(DownloadError::RedirectRefused)?
                .to_owned(),
        ];
        grant.allowed_methods = vec!["GET".to_owned()];
        grant.signed_download = None;
        grant.request_templates.clear();
        grant.propagate_trace = false;
        grant.allow_plaintext_loopback = false;
        let mut ceilings = self.ceilings.clone();
        ceilings.non_public_https = std::sync::Arc::new(Vec::new());
        let mut destination = self.storage_destination(uri, grant, ceilings).await?;
        let probe = self
            .fetch_storage(
                &mut destination,
                Coverage {
                    start: 0,
                    end_exclusive: 1,
                    total: 0,
                },
                &mut DownloadScan::default(),
                directory,
            )
            .await?;
        let probe_range = match probe {
            FetchedRange::Range { coverage, body } => {
                drop(body);
                coverage
            }
            FetchedRange::Status(status) => return storage_status(status),
        };
        let total = probe_range.total;
        let start = match window {
            Window::Tail(length) => total.saturating_sub(u64::from(length)),
            Window::Offset { start, .. } => start,
        };
        if start >= total {
            return Err(DownloadError::Incomplete);
        }
        let end = start
            .checked_add(u64::from(length))
            .ok_or(DownloadError::Incomplete)?
            .min(total);
        let coverage = Coverage {
            start,
            end_exclusive: end,
            total,
        };
        let overlap = destination
            .uri
            .expose()
            .as_str()
            .len()
            .saturating_sub(1)
            .max(
                self.credential
                    .as_ref()
                    .map_or(0, super::BoundCredential::echo_overlap),
            );
        if overlap > WINDOW_BYTES as usize {
            return Err(DownloadError::Denied);
        }
        let mut cursor = start.saturating_sub(overlap as u64);
        let checked_end = end.saturating_add(overlap as u64).min(total);
        let mut scan = DownloadScan::default();
        let mut output = directory
            .allocate()
            .await
            .map_err(|error| DownloadError::from(self.asset_error(error)))?;
        while cursor < checked_end {
            let segment_end = cursor
                .saturating_add(u64::from(WINDOW_BYTES))
                .min(checked_end);
            let checked = Coverage {
                start: cursor,
                end_exclusive: segment_end,
                total,
            };
            match self
                .fetch_storage(&mut destination, checked, &mut scan, directory)
                .await?
            {
                FetchedRange::Status(status) => return storage_status(status),
                FetchedRange::Range {
                    coverage: checked,
                    body,
                } => {
                    let visible_start = start.max(checked.start);
                    let visible_end = end.min(checked.end_exclusive);
                    if visible_start < visible_end {
                        let offset = visible_start - checked.start;
                        let count = (visible_end - visible_start) as usize;
                        let bytes = super::asset::AssetReader::output(std::sync::Arc::new(body))
                            .read(move |file| {
                                super::read_decoded(
                                    file,
                                    super::Representation::Identity,
                                    offset,
                                    count,
                                )
                                .map_err(Into::into)
                            })
                            .await
                            .map_err(|error| DownloadError::from(self.asset_error(error)))?;
                        output = output
                            .write(bytes)
                            .await
                            .map_err(|error| DownloadError::from(self.asset_error(error)))?;
                    }
                }
            }
            cursor = segment_end;
        }
        let body = output
            .finish()
            .await
            .map_err(|error| DownloadError::from(self.asset_error(error)))?;
        Ok(DownloadResponse::Storage {
            status: 206,
            coverage: Some(coverage),
            body: Some(body),
        })
    }

    async fn signed_api_request(&mut self, api_uri: String) -> Result<Response, DownloadError> {
        let span = tracing::info_span!(
            "http.request",
            "url.full" = api_uri,
            "http.request.method" = "GET",
            "server.address" = tracing::field::Empty,
            "http.response.status_code" = tracing::field::Empty,
            "dekopon.http.request.accounted_bytes" = tracing::field::Empty,
            "dekopon.http.response.accounted_bytes" = tracing::field::Empty,
            "error.code" = tracing::field::Empty,
            "error.message" = tracing::field::Empty,
            outcome = tracing::field::Empty
        );
        let index = self.evidence.len();
        let api = self
            .send_with_metadata(
                Request {
                    method: "GET".to_owned(),
                    uri: api_uri,
                    headers: vec![Header {
                        name: "user-agent".to_owned(),
                        value: b"dekopon".to_vec(),
                    }],
                    body: vec![],
                },
                super::ResponseMetadata::SignedDownload,
            )
            .instrument(span.clone())
            .await;
        self.record_request(&span, index, &api);
        api.map_err(DownloadError::from)
    }

    async fn storage_destination(
        &mut self,
        uri: Url,
        grant: HttpConstraints,
        ceilings: super::HttpHostCeilings,
    ) -> Result<Destination, DownloadError> {
        #[cfg(test)]
        if let Some(destination) = self.signed_download_destination.take() {
            if destination.uri.expose() != &uri {
                return Err(DownloadError::RedirectRefused);
            }
            return Ok(destination);
        }
        Destination::new(uri, grant, ceilings, self.deadline).await
    }

    async fn fetch_storage(
        &mut self,
        destination: &mut Destination,
        expected: Coverage,
        scan: &mut DownloadScan,
        directory: &AssetDirectory,
    ) -> Result<FetchedRange, DownloadError> {
        let index = self.evidence.len();
        let result = self
            .fetch_storage_checked(destination, expected, scan, directory)
            .await;
        self.record_storage_request(&destination.span, index, &result);
        result
    }

    async fn fetch_storage_checked(
        &mut self,
        destination: &mut Destination,
        expected: Coverage,
        scan: &mut DownloadScan,
        directory: &AssetDirectory,
    ) -> Result<FetchedRange, DownloadError> {
        let response = self
            .storage_get(destination, expected.start, expected.end_exclusive - 1)
            .await?;
        let status = response.status().as_u16();
        if status != 206 {
            return if (400..600).contains(&status) {
                Ok(FetchedRange::Status(status))
            } else {
                Err(DownloadError::Incomplete)
            };
        }
        let coverage = content_range(&response)?;
        if coverage.start != expected.start
            || coverage.end_exclusive != expected.end_exclusive
            || (expected.total != 0 && coverage.total != expected.total)
        {
            return Err(DownloadError::Incomplete);
        }
        let body = self
            .read_storage_checked(response, destination, coverage, scan, directory)
            .instrument(destination.span.clone())
            .await?;
        Ok(FetchedRange::Range { coverage, body })
    }

    fn record_storage_request<T>(
        &self,
        span: &tracing::Span,
        index: usize,
        result: &Result<T, DownloadError>,
    ) {
        let evidence = self.evidence.get(index);
        let failure = result.as_ref().err().copied();
        let outcome = failure.map_or("succeeded", DownloadError::label);
        span.record("outcome", outcome);
        if let Some(evidence) = evidence {
            span.record(
                "dekopon.http.response.accounted_bytes",
                evidence.response_bytes,
            );
        }
        if let Some(error) = failure {
            span.record("error.code", error.label());
        }
        span.in_scope(|| {
            tracing::info!(target: "dekopon_http_host::audit", {
            audit.event = "accounting.http.request",
            "http.request.method" = evidence.map(|value| value.method.as_str()),
            "server.address" = evidence.map(|value| value.authority.as_str()),
            "http.response.status_code" = evidence.and_then(|value| value.status),
            "dekopon.http.request.accounted_bytes" = evidence.map(|value| value.request_bytes),
            "dekopon.http.response.accounted_bytes" = evidence.map(|value| value.response_bytes),
            "error.code" = failure.map(DownloadError::label),
            "error.message" = failure.map(tracing::field::display), outcome,
            }, "accounting.http.request")
        });
    }

    async fn storage_get(
        &mut self,
        destination: &mut Destination,
        start: u64,
        end: u64,
    ) -> Result<reqwest::Response, DownloadError> {
        let span = tracing::info_span!(
            "http.request",
            "server.address" = destination.uri.expose().host_str(),
            "http.request.method" = "GET",
            "http.response.status_code" = tracing::field::Empty,
            "dekopon.http.response.accounted_bytes" = tracing::field::Empty,
            "error.code" = tracing::field::Empty,
            outcome = tracing::field::Empty
        );
        destination.span = span.clone();
        async {
            let grant = self.grant.as_ref().ok_or(DownloadError::Denied)?;
            if self.calls >= grant.max_requests {
                return Err(DownloadError::RequestLimit);
            }
            self.calls += 1;

            let range = format!("bytes={start}-{end}");
            let request_bytes = destination
                .request_bytes
                .checked_add((range.len() + 9) as u64)
                .ok_or(DownloadError::Denied)?;
            if request_bytes
                > self
                    .grant
                    .as_ref()
                    .ok_or(DownloadError::Denied)?
                    .max_request_bytes
            {
                return Err(DownloadError::Denied);
            }
            self.evidence.push(HttpCallEvidence {
                method: "GET".to_owned(),
                authority: destination.authority.clone(),
                status: None,
                request_bytes,
                response_bytes: 0,
                upstream_request_id: None,
                credential_injected: false,
            });
            let response = timeout_at(
                self.deadline,
                destination
                    .client
                    .request(Method::GET, destination.uri.expose().clone())
                    .headers(destination.headers.clone())
                    .header("range", range)
                    .send(),
            )
            .await
            .map_err(|_cause| DownloadError::Timeout)?
            .map_err(|_cause| DownloadError::Storage)?;
            let status = response.status().as_u16();
            tracing::Span::current().record("http.response.status_code", status);
            if let Some(evidence) = self.evidence.last_mut() {
                evidence.status = Some(status);
            }
            self.storage_head(&response)?;
            if response.status().is_redirection() {
                return Err(DownloadError::RedirectRefused);
            }
            Ok(response)
        }
        .instrument(span)
        .await
    }

    fn storage_head(&mut self, response: &reqwest::Response) -> Result<(), DownloadError> {
        let maximum = self
            .grant
            .as_ref()
            .ok_or(DownloadError::Denied)?
            .max_response_bytes;
        let bytes = super::response_head_bytes(response.headers(), &self.ceilings, maximum)
            .map_err(DownloadError::from)?;
        if let Some(evidence) = self.evidence.last_mut() {
            evidence.response_bytes = bytes;
        }
        Ok(())
    }

    #[cfg(test)]
    async fn read_storage(
        &mut self,
        response: reqwest::Response,
        destination: &Destination,
        coverage: Coverage,
        directory: &AssetDirectory,
    ) -> Result<AssetFile, DownloadError> {
        let result = self
            .read_storage_checked(
                response,
                destination,
                coverage,
                &mut DownloadScan::default(),
                directory,
            )
            .instrument(destination.span.clone())
            .await;
        destination
            .span
            .record("outcome", if result.is_ok() { "ok" } else { "error" });
        if let Some(evidence) = self.evidence.last() {
            destination.span.record(
                "dekopon.http.response.accounted_bytes",
                evidence.response_bytes,
            );
        }
        if let Err(error) = &result {
            destination
                .span
                .record("error.code", tracing::field::debug(error));
        }
        result
    }

    async fn read_storage_checked(
        &mut self,
        mut response: reqwest::Response,
        destination: &Destination,
        coverage: Coverage,
        scan: &mut DownloadScan,
        directory: &AssetDirectory,
    ) -> Result<AssetFile, DownloadError> {
        let maximum = self
            .grant
            .as_ref()
            .ok_or(DownloadError::Denied)?
            .max_response_bytes;
        if response
            .headers()
            .get_all("content-encoding")
            .iter()
            .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Err(DownloadError::Incomplete);
        }
        let expected = coverage.end_exclusive - coverage.start;
        if let Some(length) = response.content_length() {
            if length > expected {
                return Err(DownloadError::ResponseTooLarge);
            }
            if length != expected {
                return Err(DownloadError::Incomplete);
            }
        }
        if expected > u64::from(WINDOW_BYTES) {
            return Err(DownloadError::ResponseTooLarge);
        }
        let mut spool = directory
            .allocate()
            .await
            .map_err(|error| DownloadError::from(self.asset_error(error)))?;
        let mut read = 0_u64;
        let patterns = [
            destination.uri.expose().as_str().as_bytes(),
            destination
                .uri
                .expose()
                .query()
                .unwrap_or_default()
                .as_bytes(),
        ];
        let overlap = patterns
            .iter()
            .map(|pattern| pattern.len())
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        while let Some(chunk) = timeout_at(self.deadline, response.chunk())
            .await
            .map_err(|_cause| DownloadError::Timeout)?
            .map_err(|_cause| DownloadError::Incomplete)?
        {
            read = read
                .checked_add(chunk.len() as u64)
                .ok_or(DownloadError::ResponseTooLarge)?;
            let evidence = self.evidence.last_mut().ok_or(DownloadError::Internal)?;
            evidence.response_bytes = evidence
                .response_bytes
                .checked_add(chunk.len() as u64)
                .ok_or(DownloadError::ResponseTooLarge)?;
            if read > expected || evidence.response_bytes > maximum {
                return Err(DownloadError::ResponseTooLarge);
            }
            for bytes in chunk.chunks(super::CHUNK_BYTES) {
                scan.location.extend_from_slice(bytes);
                if patterns.iter().any(|pattern| {
                    !pattern.is_empty()
                        && scan
                            .location
                            .windows(pattern.len())
                            .any(|part| part == *pattern)
                }) {
                    return Err(DownloadError::Denied);
                }
                let clean = scan.location.len().saturating_sub(overlap);
                scan.location.drain(..clean);
                if let Some(credential) = &self.credential {
                    let clean = credential
                        .scan_stream_chunk(&mut scan.credential, bytes, credential.echo_overlap())
                        .ok_or(DownloadError::Denied)?;
                    scan.credential.drain(..clean);
                }
                spool = spool
                    .write(bytes.to_vec())
                    .await
                    .map_err(|error| DownloadError::from(self.asset_error(error)))?;
            }
        }
        if read != expected {
            return Err(DownloadError::Incomplete);
        }
        spool
            .finish()
            .await
            .map_err(|error| DownloadError::from(self.asset_error(error)))
    }
}

pub(super) struct ApiRedactor {
    matcher: aho_corasick::AhoCorasick,
    pending: Vec<u8>,
    overlap: usize,
}

impl ApiRedactor {
    pub(super) fn new(headers: &mut [Header]) -> Result<Self, HttpError> {
        let mut secrets = Vec::new();
        for header in headers
            .iter()
            .filter(|header| header.name.eq_ignore_ascii_case("location"))
        {
            let (path_query, fragment) = split_secret_part(&header.value, b'#');
            let (_, query) = split_secret_part(path_query, b'?');
            for part in [query, fragment]
                .into_iter()
                .flatten()
                .filter(|part| !part.is_empty())
            {
                for pair in part.split(|byte| *byte == b'&') {
                    let (key, value) = split_secret_part(pair, b'=');
                    let value = value.unwrap_or(pair);
                    if !value.is_empty() && (key.eq_ignore_ascii_case(b"sig") || value.len() >= 16)
                    {
                        secrets.push(value.to_vec());
                        secrets.push(percent_encoding::percent_decode(value).collect());
                    }
                }
            }
        }
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        let overlap = secrets
            .first()
            .map_or(0, |secret| secret.len().saturating_sub(1));
        let matcher = aho_corasick::AhoCorasick::builder()
            .match_kind(aho_corasick::MatchKind::LeftmostLongest)
            .build(&secrets)
            .map_err(|_error| {
                super::http_error(
                    ErrorCode::Internal,
                    "response redactor could not be constructed",
                )
            })?;
        redact_api_locations(headers);
        let replacements = vec![b"[redacted]"; secrets.len()];
        for header in headers {
            header.name =
                String::from_utf8(matcher.replace_all_bytes(header.name.as_bytes(), &replacements))
                    .map_err(|_error| {
                        super::http_error(ErrorCode::Internal, "redacted header name is not UTF-8")
                    })?;
            header.value = matcher.replace_all_bytes(&header.value, &replacements);
        }
        Ok(Self {
            matcher,
            pending: Vec::new(),
            overlap,
        })
    }

    pub(super) fn push(
        &mut self,
        bytes: &[u8],
        output: &mut Vec<u8>,
        maximum: u64,
    ) -> Result<(), HttpError> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() < self.overlap + super::stream::CHUNK_BYTES {
            return Ok(());
        }
        self.render(self.pending.len() - self.overlap, output, maximum)
    }

    pub(super) fn finish(&mut self, output: &mut Vec<u8>, maximum: u64) -> Result<(), HttpError> {
        self.render(self.pending.len(), output, maximum)
    }

    fn render(
        &mut self,
        ready: usize,
        output: &mut Vec<u8>,
        maximum: u64,
    ) -> Result<(), HttpError> {
        let mut consumed = 0;
        for found in self.matcher.find_iter(&self.pending) {
            if found.start() >= ready {
                break;
            }
            append_redacted(output, &self.pending[consumed..found.start()], maximum)?;
            append_redacted(output, b"[redacted]", maximum)?;
            consumed = found.end();
        }
        if consumed < ready {
            append_redacted(output, &self.pending[consumed..ready], maximum)?;
            consumed = ready;
        }
        self.pending.drain(..consumed);
        Ok(())
    }
}

fn split_secret_part(bytes: &[u8], separator: u8) -> (&[u8], Option<&[u8]>) {
    match bytes.iter().position(|byte| *byte == separator) {
        Some(index) => (&bytes[..index], Some(&bytes[index + 1..])),
        None => (bytes, None),
    }
}

fn append_redacted(output: &mut Vec<u8>, bytes: &[u8], maximum: u64) -> Result<(), HttpError> {
    if (output.len() as u64).saturating_add(bytes.len() as u64) > maximum {
        return Err(super::http_error(
            ErrorCode::ResponseTooLarge,
            "redacted response exceeds the authorized byte limit",
        ));
    }
    output.extend_from_slice(bytes);
    Ok(())
}

pub(super) fn redact_api_locations(headers: &mut [Header]) {
    for header in headers {
        if !header.name.eq_ignore_ascii_case("location") {
            continue;
        }
        let Some(index) = header
            .value
            .iter()
            .position(|byte| *byte == b'?' || *byte == b'#')
        else {
            continue;
        };
        let query = header.value.get(index) == Some(&b'?');
        let fragment = header.value.contains(&b'#');
        header.value.truncate(index);
        if query {
            header.value.extend_from_slice(b"?[redacted]");
        }
        if fragment {
            header.value.extend_from_slice(b"#[redacted]");
        }
    }
}

fn storage_status(status: u16) -> Result<DownloadResponse, DownloadError> {
    if (400..600).contains(&status) {
        Ok(DownloadResponse::Storage {
            status,
            coverage: None,
            body: None,
        })
    } else {
        Err(DownloadError::Incomplete)
    }
}

fn api_route(uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let segments = url.path().split('/').collect::<Vec<_>>();
    let [
        "",
        "repos",
        owner,
        repository,
        "actions",
        "jobs",
        job,
        "logs",
    ] = segments.as_slice()
    else {
        return false;
    };
    [owner, repository].iter().all(|value| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    }) && job.bytes().all(|byte| byte.is_ascii_digit())
        && job.parse::<u64>().is_ok_and(|id| id > 0)
}

fn location(headers: &[Header]) -> Result<Url, DownloadError> {
    let mut locations = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("location"));
    let header = locations.next().ok_or(DownloadError::RedirectRefused)?;
    if locations.next().is_some()
        || header.value.len() > LOCATION_BYTES
        || header
            .value
            .iter()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err(DownloadError::RedirectRefused);
    }
    let value =
        std::str::from_utf8(&header.value).map_err(|_cause| DownloadError::RedirectRefused)?;
    let uri = Url::parse(value).map_err(|_cause| DownloadError::RedirectRefused)?;
    if uri.scheme() != "https"
        || uri.port_or_known_default() != Some(443)
        || !uri.username().is_empty()
        || uri.password().is_some()
        || uri.fragment().is_some()
        || value.contains(['\\', '%'])
            && value.split_once("://").is_some_and(|(_, rest)| {
                rest.split('/')
                    .next()
                    .is_some_and(|host| host.contains(['\\', '%']))
            })
        || !uri.host_str().is_some_and(storage_host)
    {
        return Err(DownloadError::RedirectRefused);
    }
    Ok(uri)
}

fn storage_host(host: &str) -> bool {
    if let Some(label) = host
        .strip_suffix(".blob.core.windows.net")
        .and_then(|host| host.strip_prefix("productionresultssa"))
    {
        return (1..=5).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    }
    if let Some(label) = host
        .strip_suffix(".actions.githubusercontent.com")
        .and_then(|host| host.strip_prefix("pipelines"))
    {
        return label.len() <= 54
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.ends_with('-');
    }
    false
}

fn content_range(response: &reqwest::Response) -> Result<Coverage, DownloadError> {
    let mut values = response.headers().get_all("content-range").iter();
    let value = values
        .next()
        .and_then(|value| value.to_str().ok())
        .ok_or(DownloadError::Incomplete)?;
    if values.next().is_some() {
        return Err(DownloadError::Incomplete);
    }
    let (range, total) = value
        .strip_prefix("bytes ")
        .and_then(|value| value.split_once('/'))
        .ok_or(DownloadError::Incomplete)?;
    let (start, end) = range.split_once('-').ok_or(DownloadError::Incomplete)?;
    let parse = |value: &str| {
        if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
            value.parse::<u64>().ok()
        } else {
            None
        }
    };
    let start = parse(start).ok_or(DownloadError::Incomplete)?;
    let end_exclusive = parse(end)
        .and_then(|end| end.checked_add(1))
        .ok_or(DownloadError::Incomplete)?;
    let total = parse(total).ok_or(DownloadError::Incomplete)?;
    if start >= end_exclusive || end_exclusive > total {
        return Err(DownloadError::Incomplete);
    }
    Ok(Coverage {
        start,
        end_exclusive,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoundCredential, HttpHostCeilings};
    use dekopon_core::Redacted;
    use dekopon_test_support::LoopbackServer;
    use std::time::Duration;

    fn grant(authority: &str) -> HttpConstraints {
        HttpConstraints {
            allowed_hosts: vec![authority.to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            max_requests: 3,
            max_request_bytes: 16384,
            max_response_bytes: 131072,
            allow_plaintext_loopback: true,
            propagate_trace: false,
            request_templates: vec![],
            signed_download: Some(SignedDownload::GithubActionsLogsV1),
        }
    }

    async fn destination(server: &LoopbackServer, parent: &BufferedHttpClient) -> Destination {
        Destination::new(
            Url::parse(&format!("{}?sig=private-signed-query", server.url())).unwrap(),
            grant(server.authority()),
            HttpHostCeilings::default(),
            parent.deadline,
        )
        .await
        .unwrap()
    }

    #[test]
    fn native_locations_admit_only_the_two_bounded_host_families() {
        for (host, admitted) in [
            ("productionresultssa1.blob.core.windows.net", true),
            ("productionresultssa12345.blob.core.windows.net", true),
            ("productionresultssa123456.blob.core.windows.net", false),
            ("productionresultssa.blob.core.windows.net", false),
            ("pipelines.actions.githubusercontent.com", true),
            ("pipelines1.actions.githubusercontent.com.evil.test", false),
            ("evil.blob.core.windows.net", false),
            ("pipelines1.actions.githubusercontent.com.", false),
        ] {
            assert_eq!(storage_host(host), admitted, "{host}");
        }
        for value in [
            "http://productionresultssa1.blob.core.windows.net/log?sig=x".to_owned(),
            "https://127.0.0.1/log".to_owned(),
            "https://[::1]/log".to_owned(),
            "https://evil.test/log".to_owned(),
            "https://user@productionresultssa1.blob.core.windows.net/log".to_owned(),
            "https://productionresultssa1.blob.core.windows.net:444/log".to_owned(),
            format!(
                "https://productionresultssa1.blob.core.windows.net/{}",
                "x".repeat(8192)
            ),
        ] {
            assert_eq!(
                location(&[Header {
                    name: "location".to_owned(),
                    value: value.into_bytes()
                }]),
                Err(DownloadError::RedirectRefused)
            );
        }
    }

    #[tokio::test]
    async fn native_storage_ranges_never_receive_the_parent_credential_and_preserve_full_windows() {
        let secret = "native-api-secret-with-sixteen-bytes";
        let api = LoopbackServer::once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let credential = BoundCredential::bearer(
            "Bearer",
            Redacted::new(secret.to_owned()),
            vec![api.authority().to_owned()],
        )
        .unwrap();
        let mut parent = BufferedHttpClient::authorized_with_credential(
            grant(api.authority()),
            Some(credential),
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .unwrap();
        parent
            .send(Request {
                method: "GET".to_owned(),
                uri: api.url(),
                headers: vec![],
                body: vec![],
            })
            .await
            .unwrap();
        assert!(api.request_text().contains(secret));
        api.join();
        let body = vec![b'x'; 65536];
        let mut tail = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4464-69999/70000\r\nContent-Length: 65536\r\nX-Fixture: nonempty\r\nConnection: close\r\n\r\n".to_vec();
        tail.extend_from_slice(&body);
        let storage = LoopbackServer::sequence([
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/70000\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx".to_vec(), tail,
        ]);
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 131072);
        let mut target = destination(&storage, &parent).await;
        let deadline = parent.deadline;
        let probe = parent.storage_get(&mut target, 0, 0).await.unwrap();
        let coverage = content_range(&probe).unwrap();
        let probe = parent
            .read_storage(probe, &target, coverage, &directory)
            .await
            .unwrap();
        assert_eq!(probe.len(), 1);
        let response = parent.storage_get(&mut target, 4464, 69999).await.unwrap();
        let coverage = content_range(&response).unwrap();
        let body = parent
            .read_storage(response, &target, coverage, &directory)
            .await
            .unwrap();
        assert_eq!(body.len(), 65536);
        assert_eq!(parent.calls, 3);
        assert_eq!(parent.deadline, deadline);
        assert!(parent.evidence.last().unwrap().response_bytes > 65536);
        for expected in ["range: bytes=0-0", "range: bytes=4464-69999"] {
            let request = storage.request_text().to_ascii_lowercase();
            assert!(request.contains(expected));
            assert!(!request.contains("authorization"));
            assert!(!request.contains(secret));
            assert!(!request.contains("cookie:"));
            assert!(!request.contains("traceparent:"));
        }
        assert_eq!(
            parent.storage_get(&mut target, 0, 0).await.err(),
            Some(DownloadError::RequestLimit)
        );
        storage.join();
    }

    #[tokio::test]
    async fn native_storage_refuses_second_hops_and_bounds_hostile_or_incomplete_bodies() {
        for (response, expected) in [
            (b"HTTP/1.1 302 Found\r\nLocation: https://evil.test/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(), DownloadError::RedirectRefused),
            (b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/3\r\nContent-Length: 3\r\nConnection: close\r\n\r\nx".to_vec(), DownloadError::Incomplete),
            (b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-2/3\r\nContent-Length: 4\r\nConnection: close\r\n\r\nxxxx".to_vec(), DownloadError::ResponseTooLarge),
            (b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-23/24\r\nContent-Length: 24\r\nConnection: close\r\n\r\nsig=private-signed-query!".to_vec(), DownloadError::Denied),
        ] {
            let server = LoopbackServer::once(&response);
            let mut parent = BufferedHttpClient::authorized(grant(server.authority()), HttpHostCeilings::default(), Duration::from_secs(5)).unwrap();
            let mut target = destination(&server, &parent).await;
            let root = tempfile::tempdir().unwrap();
            let directory = AssetDirectory::new(root.path().to_owned(), 65536);
            let result = match parent.storage_get(&mut target, 0, 2).await {
                Ok(response) => {
                    let coverage = content_range(&response).unwrap();
                    parent.read_storage(response, &target, coverage, &directory).await.map(|_| ())
                }
                Err(error) => Err(error),
            };
            assert_eq!(result, Err(expected));
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
            assert!(!server.request_text().contains("authorization"));
            server.join();
        }
    }

    #[test]
    fn native_api_outcomes_and_storage_failures_remain_distinct() {
        for status in [403, 404, 410, 500] {
            match storage_status(status).unwrap() {
                DownloadResponse::Storage {
                    status: actual,
                    coverage,
                    body,
                } => {
                    assert_eq!(actual, status);
                    assert!(coverage.is_none());
                    assert!(body.is_none());
                }
                DownloadResponse::Api(_) => panic!("storage status became an API response"),
            }
        }
        assert!(!api_route("https://api.github.com/user"));
        assert!(!api_route(
            "https://api.github.com/extra/repos/owner/repo/actions/jobs/1/logs"
        ));
        assert!(!api_route(
            "https://api.github.com/repos/owner/repo/actions/jobs/+1/logs"
        ));
        assert!(api_route(
            "https://api.github.com/repos/owner/repo/actions/jobs/1/logs"
        ));
    }
    async fn tls_api(
        response: Vec<u8>,
    ) -> (String, HttpHostCeilings, tokio::task::JoinHandle<Vec<u8>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let certified =
            rcgen::generate_simple_self_signed(vec!["api.fixture.test".to_owned()]).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
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
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let authority = format!("api.fixture.test:{}", address.port());
        let pin =
            crate::LoopbackHttpsPin::new(&authority, address, certified.cert.pem().into_bytes())
                .unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(stream).await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0; 1024];
                let count = tls.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 16384);
            }
            tls.write_all(&response).await.unwrap();
            tls.shutdown().await.unwrap();
            request
        });
        (
            format!("https://{authority}/repos/owner/repo/actions/jobs/1/logs"),
            HttpHostCeilings {
                loopback_https_pin: Some(pin),
                ..HttpHostCeilings::default()
            },
            server,
        )
    }

    #[tokio::test]
    async fn native_signed_download_rejects_locations_before_any_storage_request_and_keeps_api_statuses()
     {
        for value in [
            "http://productionresultssa1.blob.core.windows.net/log".to_owned(),
            "https://evil.test/log".to_owned(),
            "https://127.0.0.1/log".to_owned(),
            "https://[::1]/log".to_owned(),
            format!(
                "https://productionresultssa1.blob.core.windows.net/{}",
                "x".repeat(8192)
            ),
        ] {
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {value}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let (uri, ceilings, server) = tls_api(response.into_bytes()).await;
            let authority = Url::parse(&uri).unwrap().authority().to_owned();
            let secret = "native-tls-api-secret-sixteen-bytes";
            let credential = BoundCredential::bearer(
                "Bearer",
                Redacted::new(secret.to_owned()),
                vec![authority.clone()],
            )
            .unwrap();
            let mut parent = BufferedHttpClient::authorized_with_credential(
                grant(&authority),
                Some(credential),
                ceilings,
                Duration::from_secs(5),
            )
            .unwrap();
            let root = tempfile::tempdir().unwrap();
            let result = parent
                .signed_download(
                    uri,
                    Window::Tail(65536),
                    &AssetDirectory::new(root.path().to_owned(), 131072),
                )
                .await;
            assert_eq!(result.err(), Some(DownloadError::RedirectRefused));
            assert_eq!(
                parent.policy_violation().map(crate::PolicyViolation::label),
                Some("redirect-refused")
            );
            assert_eq!(parent.calls, 1);
            assert_eq!(parent.evidence.len(), 1);
            let request = String::from_utf8(server.await.unwrap()).unwrap();
            assert!(request.contains(secret));
            assert!(request.to_ascii_lowercase().contains("user-agent: dekopon"));
            assert!(request.starts_with("GET /repos/owner/repo/actions/jobs/1/logs HTTP/1.1"));
        }
        for status in [403, 404, 410] {
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Length: 3\r\nX-Fixture: safe\r\nConnection: close\r\n\r\nraw"
            );
            let (uri, ceilings, server) = tls_api(response.into_bytes()).await;
            let authority = Url::parse(&uri).unwrap().authority().to_owned();
            let credential = BoundCredential::bearer(
                "Bearer",
                Redacted::new("api-test-credential-sixteen-bytes".to_owned()),
                vec![authority.clone()],
            )
            .unwrap();
            let mut parent = BufferedHttpClient::authorized_with_credential(
                grant(&authority),
                Some(credential),
                ceilings,
                Duration::from_secs(5),
            )
            .unwrap();
            let root = tempfile::tempdir().unwrap();
            let response = parent
                .signed_download(
                    uri,
                    Window::Tail(65536),
                    &AssetDirectory::new(root.path().to_owned(), 131072),
                )
                .await
                .unwrap();
            match response {
                DownloadResponse::Api(response) => {
                    assert_eq!(response.status, status);
                    assert_eq!(response.body, b"raw");
                }
                DownloadResponse::Storage { .. } => panic!("API status became a storage status"),
            }
            assert_eq!(parent.calls, 1);
            server.await.unwrap();
        }
    }
    #[tokio::test]
    async fn native_signed_storage_traces_exclude_signed_urls_and_keep_opaque_bytes() {
        use tracing::instrument::WithSubscriber as _;
        use tracing_subscriber::layer::SubscriberExt as _;
        let captured = dekopon_test_support::CaptureLayer::workspace();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        async {
            let response = b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/4\r\nContent-Length: 4\r\nConnection: close\r\n\r\n\xff\x1b\0\n";
            let server = LoopbackServer::once(response);
            let mut parent = BufferedHttpClient::authorized(grant(server.authority()), HttpHostCeilings::default(), Duration::from_secs(5)).unwrap();
            let mut target = destination(&server, &parent).await;
            let root = tempfile::tempdir().unwrap();
            let response = parent.storage_get(&mut target, 0, 3).await.unwrap();
            let coverage = content_range(&response).unwrap();
            let body = parent.read_storage(response, &target, coverage, &AssetDirectory::new(root.path().to_owned(), 65536)).await.unwrap();
            let bytes = crate::read_decoded(body.file(), crate::Representation::Identity, 0, 4).unwrap();
            assert_eq!(bytes, b"\xff\x1b\0\n");
            assert!(server.request_text().contains("sig=private-signed-query"));
            server.join();
        }.with_subscriber(subscriber).await;
        let recorded = captured.text();
        assert!(recorded.contains("http.request"));
        assert!(recorded.contains("206"));
        assert!(!recorded.contains("private-signed-query"));
        assert!(!recorded.contains("?sig="));
    }

    #[tokio::test]
    async fn native_signed_download_requires_its_separate_grant_before_api_egress() {
        let mut constraints = grant("api.github.com");
        constraints.signed_download = None;
        let mut parent = BufferedHttpClient::authorized(
            constraints,
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let result = parent
            .signed_download(
                "https://api.github.com/repos/owner/repo/actions/jobs/1/logs".to_owned(),
                Window::Tail(65536),
                &AssetDirectory::new(root.path().to_owned(), 65536),
            )
            .await;
        assert_eq!(result.err(), Some(DownloadError::Denied));
        assert_eq!(parent.calls, 0);
        assert!(parent.evidence.is_empty());
        assert_eq!(
            parent.policy_violation(),
            Some(crate::PolicyViolation::Denied)
        );
    }
    fn authenticated_api(uri: &str, ceilings: HttpHostCeilings) -> BufferedHttpClient {
        let authority = Url::parse(uri).unwrap().authority().to_owned();
        let credential = BoundCredential::bearer(
            "Bearer",
            Redacted::new("api-credential-with-sixteen-bytes".to_owned()),
            vec![authority.clone()],
        )
        .unwrap();
        BufferedHttpClient::authorized_with_credential(
            grant(&authority),
            Some(credential),
            ceilings,
            Duration::from_secs(5),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn native_signed_api_requires_authentication_before_dispatch() {
        let (uri, ceilings, server) =
            tls_api(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec())
                .await;
        let authority = Url::parse(&uri).unwrap().authority().to_owned();
        let mut parent =
            BufferedHttpClient::authorized(grant(&authority), ceilings, Duration::from_secs(5))
                .unwrap();
        let root = tempfile::tempdir().unwrap();
        let result = parent
            .signed_download(
                uri,
                Window::Tail(64),
                &AssetDirectory::new(root.path().to_owned(), 131072),
            )
            .await;
        assert_eq!(result.err(), Some(DownloadError::Denied));
        assert_eq!(parent.calls, 0);
        assert!(parent.evidence.is_empty());
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn native_signed_302_never_stores_reflected_request_ids_even_when_the_redirect_is_refused()
     {
        for length in [0, 1] {
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://productionresultssa1.blob.core.windows.net/log?sig=private-signed-query\r\nx-github-request-id: sig=private-signed-query\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
            );
            let (uri, ceilings, server) = tls_api(response.into_bytes()).await;
            let mut parent = authenticated_api(&uri, ceilings);
            let root = tempfile::tempdir().unwrap();
            assert!(
                parent
                    .signed_download(
                        uri,
                        Window::Tail(64),
                        &AssetDirectory::new(root.path().to_owned(), 131072)
                    )
                    .await
                    .is_err()
            );
            assert_eq!(parent.evidence.len(), 1);
            assert_eq!(parent.evidence[0].upstream_request_id, None);
            assert!(
                !serde_json::to_string(&parent.evidence)
                    .unwrap()
                    .contains("private-signed-query")
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn native_signed_api_redacts_non_302_locations_without_following_them() {
        let location = "https://productionresultssa1.blob.core.windows.net/log?sig=raw-api-location#private-fragment";
        for status in [301, 303, 307] {
            let response = format!(
                "HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nContent-Length: 3\r\nConnection: close\r\n\r\nraw"
            );
            let (uri, ceilings, server) = tls_api(response.into_bytes()).await;
            let mut parent = authenticated_api(&uri, ceilings);
            let root = tempfile::tempdir().unwrap();
            match parent
                .signed_download(
                    uri,
                    Window::Tail(64),
                    &AssetDirectory::new(root.path().to_owned(), 131072),
                )
                .await
                .unwrap()
            {
                DownloadResponse::Api(response) => {
                    assert_eq!(response.status, status);
                    assert_eq!(response.body, b"raw");
                    assert!(
                        response
                            .headers
                            .iter()
                            .any(|header| header.name == "location"
                                && header.value == b"https://productionresultssa1.blob.core.windows.net/log?[redacted]#[redacted]")
                    );
                }
                DownloadResponse::Storage { .. } => panic!("non-302 API response followed storage"),
            }
            assert_eq!(parent.calls, 1);
            assert_eq!(parent.policy_violation(), None);
            server.await.unwrap();
        }
    }
    #[test]
    fn signed_api_redaction_preserves_matches_across_every_chunk_boundary() {
        let source = b"before private-query-long private-query private-fragment after";
        for split in 0..=source.len() {
            let mut headers = vec![Header {
                name: "location".to_owned(),
                value: b"https://storage.test/log?sig=private-query&other=private-query-long#private-fragment".to_vec(),
            }];
            let mut redactor = ApiRedactor::new(&mut headers).unwrap();
            let mut output = Vec::new();
            let prefix = vec![b'.'; super::super::stream::CHUNK_BYTES - split];
            redactor.push(&prefix, &mut output, 131072).unwrap();
            redactor
                .push(&source[..split], &mut output, 131072)
                .unwrap();
            redactor
                .push(&source[split..], &mut output, 131072)
                .unwrap();
            redactor.finish(&mut output, 131072).unwrap();
            let mut expected = prefix;
            expected.extend_from_slice(b"before [redacted] [redacted] [redacted] after");
            assert_eq!(output, expected);
            assert!(redactor.pending.is_empty());
        }
    }

    #[test]
    fn signed_api_redaction_bounds_substitution_expansion() {
        let mut headers = vec![Header {
            name: "location".to_owned(),
            value: b"https://storage.test/log?sig=x".to_vec(),
        }];
        let mut redactor = ApiRedactor::new(&mut headers).unwrap();
        let mut output = Vec::new();
        redactor.push(b"xxxx", &mut output, 16).unwrap();
        assert_eq!(
            redactor.finish(&mut output, 16).unwrap_err().code,
            ErrorCode::ResponseTooLarge
        );
        assert!(output.len() <= 16);
    }

    async fn reflected_api(status: u16) -> (Response, Vec<HttpCallEvidence>) {
        let location = "https://productionresultssa1.blob.core.windows.net/log?sig=private-query&other=private-query-long#private-fragment";
        let body =
            format!("<a href=\"{location}\">download</a> private-query-long private-fragment");
        let wire = format!(
            "HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nx-reflection: private-query-long private-fragment\r\nprivate-fragment: safe\r\nx-github-request-id: private-query\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (uri, ceilings, server) = tls_api(wire.into_bytes()).await;
        let mut parent = authenticated_api(&uri, ceilings);
        let root = tempfile::tempdir().unwrap();
        let result = parent
            .signed_download(
                uri,
                Window::Tail(64),
                &AssetDirectory::new(root.path().to_owned(), 131072),
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(parent.calls, 1);
        match result {
            DownloadResponse::Api(response) => (response, parent.evidence),
            DownloadResponse::Storage { .. } => panic!("non-302 API response followed storage"),
        }
    }

    #[tokio::test]
    async fn a_short_sas_parameter_is_not_scrubbed_from_a_redirect_body() {
        let body = format!("<html>{} reflected token</html>", "br br br ".repeat(64));
        let expected = body.replace("token", "[redacted]");
        let wire = format!(
            "HTTP/1.1 301 Redirect\r\nLocation: https://productionresultssa1.blob.core.windows.net/log?sp=r&sr=b&SiG=token\r\nx-brown-bird: br\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (uri, ceilings, server) = tls_api(wire.into_bytes()).await;
        let mut parent = authenticated_api(&uri, ceilings);
        parent.grant.as_mut().unwrap().max_response_bytes = 1024;
        let root = tempfile::tempdir().unwrap();
        let response = parent
            .signed_download(
                uri,
                Window::Tail(64),
                &AssetDirectory::new(root.path().to_owned(), 1024),
            )
            .await
            .unwrap();
        match response {
            DownloadResponse::Api(response) => {
                assert_eq!(response.status, 301);
                assert_eq!(response.body, expected.as_bytes());
                assert!(
                    response
                        .headers
                        .iter()
                        .any(|header| { header.name == "x-brown-bird" && header.value == b"br" })
                );
                assert!(response.headers.iter().any(|header| {
                    header.name == "location"
                        && header.value
                            == b"https://productionresultssa1.blob.core.windows.net/log?[redacted]"
                }));
            }
            DownloadResponse::Storage { .. } => panic!("301 response followed storage"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_signed_redirect_body_substitutes_location_secrets_and_preserves_html() {
        for status in [301, 303, 307] {
            let (response, _) = reflected_api(status).await;
            assert_eq!(response.status, status);
            assert_eq!(response.body, b"<a href=\"https://productionresultssa1.blob.core.windows.net/log?sig=[redacted]&other=[redacted]#[redacted]\">download</a> [redacted] [redacted]");
        }
    }

    #[tokio::test]
    async fn native_signed_redirect_sibling_headers_substitute_location_secrets_longest_first() {
        let (response, _) = reflected_api(303).await;
        let reflection = response
            .headers
            .iter()
            .find(|header| header.name == "x-reflection")
            .unwrap();
        assert_eq!(reflection.value, b"[redacted] [redacted]");
        assert!(
            response
                .headers
                .iter()
                .any(|header| header.name == "[redacted]" && header.value == b"safe")
        );
        let request_id = response
            .headers
            .iter()
            .find(|header| header.name == "x-github-request-id")
            .unwrap();
        assert_eq!(request_id.value, b"[redacted]");
    }

    #[tokio::test]
    async fn native_signed_redirect_evidence_never_retains_response_request_ids() {
        for status in [200, 301, 303, 307, 403] {
            let (_, evidence) = reflected_api(status).await;
            assert_eq!(evidence.len(), 1);
            assert_eq!(evidence[0].upstream_request_id, None);
            assert!(
                !serde_json::to_string(&evidence)
                    .unwrap()
                    .contains("private-query")
            );
        }
    }

    async fn fake_storage(
        data: Vec<u8>,
        count: usize,
    ) -> (Destination, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let hostname = "productionresultssa1.blob.core.windows.net";
        let certified = rcgen::generate_simple_self_signed(vec![hostname.to_owned()]).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
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
        let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .add_root_certificate(
                reqwest::Certificate::from_pem(certified.cert.pem().as_bytes()).unwrap(),
            )
            .resolve(hostname, address)
            .build()
            .unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..count {
                let (stream, _) = listener.accept().await.unwrap();
                let mut tls = acceptor.accept(stream).await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0; 1024];
                    let length = tls.read(&mut bytes).await.unwrap();
                    assert_ne!(length, 0);
                    request.extend_from_slice(&bytes[..length]);
                    if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8(request.clone())
                    .unwrap()
                    .to_ascii_lowercase();
                let range = text
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start = start.parse::<usize>().unwrap();
                let end = end.parse::<usize>().unwrap();
                assert!(start <= end && end < data.len());
                let response = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len(),
                    end - start + 1
                );
                tls.write_all(response.as_bytes()).await.unwrap();
                tls.write_all(&data[start..=end]).await.unwrap();
                tls.shutdown().await.unwrap();
                requests.push(request);
            }
            requests
        });
        let uri = Url::parse(&format!("https://{hostname}/log?sig=private-signed-query")).unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "accept-encoding",
            reqwest::header::HeaderValue::from_static("identity"),
        );
        headers.insert(
            "user-agent",
            reqwest::header::HeaderValue::from_static("dekopon"),
        );
        (
            Destination {
                uri: Redacted::new(uri),
                client,
                headers,
                authority: hostname.to_owned(),
                request_bytes: 256,
                span: tracing::Span::none(),
            },
            server,
        )
    }

    #[tokio::test]
    async fn native_302_to_storage_preserves_credentials_secrecy_coverage_and_every_accounting_event()
     {
        use tracing::instrument::WithSubscriber as _;
        use tracing_subscriber::layer::SubscriberExt as _;
        let capture = dekopon_test_support::CaptureLayer::workspace();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let evidence = async {
            let (destination, storage) = fake_storage(vec![b'x'; 70000], 3).await;
            let response = format!("HTTP/1.1 302 Found\r\nLocation: {}\r\nx-github-request-id: sig=private-signed-query\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", destination.uri.expose());
            let (uri, ceilings, api) = tls_api(response.into_bytes()).await;
            let mut parent = authenticated_api(&uri, ceilings);
            parent.grant.as_mut().unwrap().max_requests = 8;
            parent.signed_download_destination = Some(destination);
            let root = tempfile::tempdir().unwrap();
            let result = parent.signed_download(uri, Window::Tail(65536), &AssetDirectory::new(root.path().to_owned(), 131072)).await.unwrap();
            match result {
                DownloadResponse::Storage { status, coverage, body } => {
                    assert_eq!(status, 206);
                    assert_eq!(coverage, Some(Coverage { start: 4464, end_exclusive: 70000, total: 70000 }));
                    assert_eq!(body.unwrap().len(), 65536);
                }
                DownloadResponse::Api(_) => panic!("302 was not followed"),
            }
            let api_request = String::from_utf8(api.await.unwrap()).unwrap();
            assert!(api_request.contains("authorization: Bearer api-credential-with-sixteen-bytes"));
            let requests = storage.await.unwrap();
            assert_eq!(requests.len(), 3);
            for request in requests {
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(!request.contains("authorization:"));
                assert!(!request.contains("api-credential-with-sixteen-bytes"));
                assert!(!request.contains("traceparent:"));
            }
            assert_eq!(parent.calls, 4);
            parent.evidence
        }.with_subscriber(subscriber).await;
        let events = capture.events_where(
            "dekopon_http_host::audit",
            &[("audit.event", "accounting.http.request")],
        );
        assert_eq!(events.len(), 4);
        for (event, evidence) in events.iter().zip(&evidence) {
            assert_eq!(
                event
                    .field("dekopon.http.response.accounted_bytes")
                    .unwrap()
                    .parse::<u64>()
                    .unwrap(),
                evidence.response_bytes
            );
        }
        assert!(
            evidence
                .iter()
                .all(|call| call.upstream_request_id.is_none())
        );
        assert!(!capture.text().contains("private-signed-query"));
        assert!(
            !serde_json::to_string(&evidence)
                .unwrap()
                .contains("private-signed-query")
        );
    }

    #[tokio::test]
    async fn native_offset_reads_cannot_reconstruct_a_signed_query_from_two_fragments() {
        let query = b"sig=private-signed-query";
        for window in [
            Window::Offset {
                start: 0,
                length: 12,
            },
            Window::Offset {
                start: 12,
                length: 11,
            },
        ] {
            let (destination, storage) = fake_storage(query.to_vec(), 2).await;
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                destination.uri.expose()
            );
            let (uri, ceilings, api) = tls_api(response.into_bytes()).await;
            let mut parent = authenticated_api(&uri, ceilings);
            parent.grant.as_mut().unwrap().max_requests = 8;
            parent.signed_download_destination = Some(destination);
            let root = tempfile::tempdir().unwrap();
            let result = parent
                .signed_download(
                    uri,
                    window,
                    &AssetDirectory::new(root.path().to_owned(), 131072),
                )
                .await;
            assert_eq!(result.err(), Some(DownloadError::Denied));
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
            assert_eq!(parent.calls, 3);
            api.await.unwrap();
            assert_eq!(storage.await.unwrap().len(), 2);
        }
    }
    #[tokio::test]
    async fn native_signed_windows_are_not_exposed_when_context_exhausts_the_request_budget() {
        let (destination, storage) = fake_storage(vec![b'x'; 70000], 1).await;
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            destination.uri.expose()
        );
        let (uri, ceilings, api) = tls_api(response.into_bytes()).await;
        let mut parent = authenticated_api(&uri, ceilings);
        parent.grant.as_mut().unwrap().max_requests = 2;
        parent.signed_download_destination = Some(destination);
        let root = tempfile::tempdir().unwrap();
        let result = parent
            .signed_download(
                uri,
                Window::Tail(65536),
                &AssetDirectory::new(root.path().to_owned(), 131072),
            )
            .await;
        assert_eq!(result.err(), Some(DownloadError::RequestLimit));
        assert_eq!(
            parent.policy_violation(),
            Some(crate::PolicyViolation::HostCallLimit)
        );
        assert_eq!(parent.calls, 2);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        api.await.unwrap();
        assert_eq!(storage.await.unwrap().len(), 1);
    }
}
