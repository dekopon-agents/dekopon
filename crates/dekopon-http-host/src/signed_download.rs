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

struct Destination {
    uri: Url,
    transport: BufferedHttpClient,
    grant: HttpConstraints,
}

impl BufferedHttpClient {
    pub async fn signed_download(
        &mut self,
        api_uri: String,
        window: Window,
        directory: &AssetDirectory,
    ) -> Result<DownloadResponse, DownloadError> {
        self.attempted = true;
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
        let span = tracing::info_span!(
            "http.request",
            "http.request.method" = "GET",
            outcome = tracing::field::Empty
        );
        let index = self.evidence.len();
        let api = self
            .send_checked(Request {
                method: "GET".to_owned(),
                uri: api_uri,
                headers: vec![],
                body: vec![],
            })
            .instrument(span.clone())
            .await;
        self.record_request(&span, index, &api);
        let api = api.map_err(DownloadError::from)?;
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
        let mut transport = Self::authorized(grant.clone(), ceilings, self.timeout)
            .map_err(|_| DownloadError::Internal)?;
        transport.deadline = self.deadline;
        let mut destination = Destination {
            uri,
            transport,
            grant,
        };
        let probe = self.storage_get(&mut destination, 0, 0).await?;
        if probe.status().as_u16() != 206 {
            return storage_status(probe.status().as_u16());
        }
        let probe_range = content_range(&probe)?;
        if probe_range.start != 0 || probe_range.end_exclusive != 1 {
            return Err(DownloadError::Incomplete);
        }
        self.read_storage(probe, &destination, probe_range, directory)
            .await?;
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
        let response = self.storage_get(&mut destination, start, end - 1).await?;
        if response.status().as_u16() != 206 {
            return storage_status(response.status().as_u16());
        }
        let coverage = content_range(&response)?;
        if coverage
            != (Coverage {
                start,
                end_exclusive: end,
                total,
            })
        {
            return Err(DownloadError::Incomplete);
        }
        let body = self
            .read_storage(response, &destination, coverage, directory)
            .await?;
        Ok(DownloadResponse::Storage {
            status: 206,
            coverage: Some(coverage),
            body: Some(body),
        })
    }

    async fn storage_get(
        &mut self,
        destination: &mut Destination,
        start: u64,
        end: u64,
    ) -> Result<reqwest::Response, DownloadError> {
        let grant = self.grant.as_ref().ok_or(DownloadError::Denied)?;
        if self.calls >= grant.max_requests {
            return Err(DownloadError::RequestLimit);
        }
        self.calls += 1;
        let span = tracing::info_span!(
            "http.request",
            "server.address" = destination.uri.host_str(),
            "http.request.method" = "GET",
            "http.response.status_code" = tracing::field::Empty
        );
        async {
            let prepared = destination
                .transport
                .prepare(
                    Request {
                        method: "GET".to_owned(),
                        uri: destination.uri.as_str().to_owned(),
                        headers: vec![
                            Header {
                                name: "range".to_owned(),
                                value: format!("bytes={start}-{end}").into_bytes(),
                            },
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
                    &destination.grant,
                    None,
                )
                .await
                .map_err(DownloadError::from)?;
            self.evidence.push(HttpCallEvidence {
                method: "GET".to_owned(),
                authority: prepared.authority,
                status: None,
                request_bytes: prepared.request_bytes,
                response_bytes: 0,
                upstream_request_id: None,
                credential_injected: false,
            });
            let client = destination
                .transport
                .pinned_client(
                    &prepared.host,
                    &prepared.addresses,
                    self.remaining().map_err(DownloadError::from)?,
                )
                .map_err(DownloadError::from)?;
            // This transport is constructed without a credential and never enters the injector.
            let response = timeout_at(
                self.deadline,
                client
                    .request(Method::GET, prepared.url)
                    .headers(prepared.headers)
                    .send(),
            )
            .await
            .map_err(|_| DownloadError::Timeout)?
            .map_err(|_| DownloadError::Storage)?;
            let status = response.status().as_u16();
            tracing::Span::current().record("http.response.status_code", status);
            if let Some(evidence) = self.evidence.last_mut() {
                evidence.status = Some(status);
            }
            if response.status().is_redirection() {
                return Err(DownloadError::RedirectRefused);
            }
            self.storage_head(&response)?;
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
        if response.headers().len() > self.ceilings.max_headers {
            return Err(DownloadError::ResponseTooLarge);
        }
        let mut bytes = 16_u64;
        for (name, value) in response.headers() {
            bytes = bytes
                .checked_add((name.as_str().len() + value.len() + 4) as u64)
                .ok_or(DownloadError::ResponseTooLarge)?;
            if bytes > maximum || bytes > self.ceilings.max_header_bytes as u64 {
                return Err(DownloadError::ResponseTooLarge);
            }
        }
        if response.headers().contains_key("content-encoding") {
            return Err(DownloadError::Incomplete);
        }
        if let Some(evidence) = self.evidence.last_mut() {
            evidence.response_bytes = bytes;
        }
        Ok(())
    }

    async fn read_storage(
        &mut self,
        mut response: reqwest::Response,
        destination: &Destination,
        coverage: Coverage,
        directory: &AssetDirectory,
    ) -> Result<AssetFile, DownloadError> {
        let maximum = self
            .grant
            .as_ref()
            .ok_or(DownloadError::Denied)?
            .max_response_bytes;
        let expected = coverage.end_exclusive - coverage.start;
        if expected > u64::from(WINDOW_BYTES) {
            return Err(DownloadError::ResponseTooLarge);
        }
        let mut spool = directory
            .allocate()
            .await
            .map_err(|error| DownloadError::from(self.asset_error(error)))?;
        let mut read = 0_u64;
        let mut scan = Vec::new();
        let patterns = [
            destination.uri.as_str().as_bytes(),
            destination.uri.query().unwrap_or_default().as_bytes(),
        ];
        let overlap = patterns
            .iter()
            .map(|pattern| pattern.len())
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        let mut credential_scan = Vec::new();
        while let Some(chunk) = timeout_at(self.deadline, response.chunk())
            .await
            .map_err(|_| DownloadError::Timeout)?
            .map_err(|_| DownloadError::Incomplete)?
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
                scan.extend_from_slice(bytes);
                if patterns.iter().any(|pattern| {
                    !pattern.is_empty() && scan.windows(pattern.len()).any(|part| part == *pattern)
                }) {
                    return Err(DownloadError::Denied);
                }
                let clean = scan.len().saturating_sub(overlap);
                scan.drain(..clean);
                if let Some(credential) = &self.credential {
                    let clean = credential
                        .scan_stream_chunk(&mut credential_scan, bytes, credential.echo_overlap())
                        .ok_or(DownloadError::Denied)?;
                    credential_scan.drain(..clean);
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
    let Some(route) = segments.windows(7).last() else {
        return false;
    };
    route[0] == "repos"
        && route[3] == "actions"
        && route[4] == "jobs"
        && route[6] == "logs"
        && [route[1], route[2]].iter().all(|value| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        && route[5].parse::<u64>().is_ok_and(|id| id > 0)
}

fn location(headers: &[Header]) -> Result<Url, DownloadError> {
    let mut locations = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("location"));
    let header = locations.next().ok_or(DownloadError::RedirectRefused)?;
    if locations.next().is_some()
        || header.value.len() > LOCATION_BYTES
        || header.value.iter().any(|b| b.is_ascii_control())
    {
        return Err(DownloadError::RedirectRefused);
    }
    let value = std::str::from_utf8(&header.value).map_err(|_| DownloadError::RedirectRefused)?;
    let uri = Url::parse(value).map_err(|_| DownloadError::RedirectRefused)?;
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

    fn destination(server: &LoopbackServer, parent: &BufferedHttpClient) -> Destination {
        let grant = grant(server.authority());
        let mut transport = BufferedHttpClient::authorized(
            grant.clone(),
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .unwrap();
        transport.deadline = parent.deadline;
        Destination {
            uri: Url::parse(&format!("{}?sig=private-signed-query", server.url())).unwrap(),
            transport,
            grant,
        }
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
        let mut target = destination(&storage, &parent);
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
            let mut target = destination(&server, &parent);
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
        assert!(api_route(
            "https://api.github.com/repos/owner/repo/actions/jobs/1/logs"
        ));
    }
}
