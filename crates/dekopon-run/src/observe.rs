//! A bounded client for OpenObserve's log search API.
//!
//! The runner and the gateway export their transcripts as OTLP log records; this is the half that
//! reads them back, so an operator can list the sessions a deployment ran and replay one. It
//! speaks the receiver's search endpoint and nothing else — no ingestion, no stream management —
//! and it treats what comes back as untrusted data: every response is byte-bounded, every page
//! count is capped, and the records are handed to `dekopon-agent`'s reconstruction as JSON values
//! it inspects field by field.
//!
//! The base URL is the same organization base the OTLP exporter posts to, so one deployment's
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is also its query base. The credential is an `Authorization`
//! header value read from an environment variable named on the command line, following the rule
//! every other Dekopon credential follows: a name may appear in an argument, a value never does.

use std::{io::Read as _, time::Duration};

use dekopon_core::Redacted;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use ureq::Agent;

/// Records fetched per request.
pub const PAGE_SIZE: usize = 500;
/// Pages one search will follow before it stops and says so.
///
/// Ten thousand records covers weeks of a small deployment's accounting and any one session's
/// transcript many times over; a search that needs more is a query to narrow, not a bound to raise.
pub const MAX_PAGES: usize = 20;
/// Bytes one search response may occupy.
const MAX_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

/// Where to search and how to authenticate.
pub struct OpenObserveSettings {
    /// Organization base URL, such as `http://127.0.0.1:5080/api/default`.
    pub url: String,
    /// The log stream the exporters wrote to.
    pub stream: String,
    /// The complete `Authorization` header value, such as `Basic <token>`.
    pub authorization: String,
    /// Whole-request deadline.
    pub timeout: Duration,
}

/// One search, paginated to its end or to [`MAX_PAGES`].
#[derive(Debug, Default)]
pub struct SearchResult {
    /// Every record the pages returned, in the order the receiver returned them.
    pub hits: Vec<Value>,
    /// Whether the page cap stopped the search before the receiver ran out of records.
    pub truncated: bool,
}

/// A client for one receiver, one stream, and one credential.
pub struct OpenObserveClient {
    agent: Agent,
    search_url: String,
    stream: String,
    authorization: Redacted<String>,
}

impl OpenObserveClient {
    /// Validates the settings and builds the client; nothing is sent.
    ///
    /// # Errors
    ///
    /// Returns [`ObserveError::Configuration`] for a blank or userinfo-bearing URL, a stream name
    /// outside `[A-Za-z0-9_]`, a blank credential, or a zero timeout.
    pub fn new(settings: OpenObserveSettings) -> Result<Self, ObserveError> {
        let url = settings.url.trim().trim_end_matches('/');
        if url.is_empty() {
            return Err(ObserveError::Configuration(
                "OpenObserve URL must not be empty".to_owned(),
            ));
        }
        if url.contains(['?', '#']) {
            return Err(ObserveError::Configuration(
                "OpenObserve URL must be an organization base without a query or fragment"
                    .to_owned(),
            ));
        }
        let authority = url
            .split_once("://")
            .map_or(url, |(_, rest)| rest)
            .split('/')
            .next()
            .unwrap_or_default();
        if authority.contains('@') {
            return Err(ObserveError::Configuration(
                "OpenObserve URL must not carry username/password userinfo; name the credential variable instead"
                    .to_owned(),
            ));
        }
        let stream = settings.stream.trim();
        if stream.is_empty()
            || !stream
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(ObserveError::Configuration(format!(
                "OpenObserve stream name {stream:?} must contain only letters, digits, and underscores"
            )));
        }
        if settings.authorization.trim().is_empty() {
            return Err(ObserveError::Configuration(
                "OpenObserve authorization header value must not be blank".to_owned(),
            ));
        }
        if settings.timeout.is_zero() {
            return Err(ObserveError::Configuration(
                "OpenObserve timeout must be greater than zero".to_owned(),
            ));
        }
        // No redirects, so the credential header cannot be forwarded to a host nobody named; no
        // ambient proxy, for the same reason `dekopon-model` refuses one.
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(settings.timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .proxy(None)
            .build()
            .into();
        Ok(Self {
            agent,
            search_url: format!("{url}/_search?type=logs"),
            stream: stream.to_owned(),
            authorization: Redacted::new(settings.authorization.trim().to_owned()),
        })
    }

    /// The SQL selecting every record one trace exported.
    ///
    /// # Errors
    ///
    /// Returns [`ObserveError::Configuration`] for a trace identifier outside the characters a
    /// trace identifier can contain, which is what keeps the interpolation a lookup and not a
    /// query the caller wrote.
    pub fn trace_sql(&self, trace_id: &str) -> Result<String, ObserveError> {
        let trace_id = trace_id.trim();
        let valid = !trace_id.is_empty()
            && trace_id.len() <= 128
            && trace_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !valid {
            return Err(ObserveError::Configuration(format!(
                "trace identifier {trace_id:?} must be 1-128 letters, digits, '-', '_', or '.'"
            )));
        }
        Ok(format!(
            "SELECT * FROM \"{}\" WHERE trace_id = '{trace_id}'",
            self.stream
        ))
    }

    /// The SQL selecting every model-turn accounting record, newest first.
    ///
    /// OpenObserve stores an attribute named `audit.event` under `audit_event`: its field names are
    /// restricted to letters, digits, and underscores, and every other character is folded to one.
    #[must_use]
    pub fn accounting_sql(&self) -> String {
        format!(
            "SELECT * FROM \"{}\" WHERE audit_event = 'accounting.model.turn' ORDER BY _timestamp DESC",
            self.stream
        )
    }

    /// Runs one SQL search over `[start_us, end_us)`, following pages to the end.
    ///
    /// # Errors
    ///
    /// Returns [`ObserveError`] when the receiver cannot be reached, answers with a failure
    /// status, or returns a body that is not the search response shape.
    pub fn search(
        &self,
        sql: &str,
        start_us: i64,
        end_us: i64,
    ) -> Result<SearchResult, ObserveError> {
        let mut result = SearchResult::default();
        for page in 0..MAX_PAGES {
            let body = json!({
                "query": {
                    "sql": sql,
                    "start_time": start_us,
                    "end_time": end_us,
                    "from": page * PAGE_SIZE,
                    "size": PAGE_SIZE
                }
            });
            let response = self
                .agent
                .post(&self.search_url)
                .header("accept", "application/json")
                // The one place the credential leaves its wrapper, straight onto the wire.
                .header("authorization", self.authorization.expose())
                .send_json(&body)
                .map_err(|error| ObserveError::Request(error.to_string()))?;
            let status = response.status().as_u16();
            let mut text = String::new();
            response
                .into_body()
                .into_reader()
                .take(MAX_RESPONSE_BYTES)
                .read_to_string(&mut text)
                .map_err(|error| ObserveError::Response(error.to_string()))?;
            if !(200..300).contains(&status) {
                return Err(ObserveError::Status {
                    status,
                    detail: sanitize(&text),
                });
            }
            let page_result = serde_json::from_str::<SearchResponse>(&text)
                .map_err(|error| ObserveError::Response(error.to_string()))?;
            let received = page_result.hits.len();
            result.hits.extend(page_result.hits);
            if received < PAGE_SIZE {
                return Ok(result);
            }
        }
        result.truncated = true;
        Ok(result)
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    hits: Vec<Value>,
}

/// Bounds and control-strips a failure body so a receiver cannot forge log structure or flood
/// stderr; the useful part of an error is its first line.
fn sanitize(text: &str) -> String {
    let cleaned = text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .take(1024)
        .collect::<String>();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "no response body".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Failure to query the receiver.
#[derive(Debug, Error)]
pub enum ObserveError {
    /// Settings were rejected before anything was sent.
    #[error("invalid OpenObserve configuration: {0}")]
    Configuration(String),
    /// The request could not be sent or answered.
    #[error("OpenObserve request failed: {0}")]
    Request(String),
    /// The receiver answered with a failure status.
    #[error("OpenObserve answered HTTP {status}: {detail}")]
    Status {
        /// The HTTP status.
        status: u16,
        /// The bounded, sanitized body.
        detail: String,
    },
    /// The response was not a search result.
    #[error("OpenObserve answered with something other than a search result: {0}")]
    Response(String),
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ObserveError, OpenObserveClient, OpenObserveSettings};

    fn settings(url: &str) -> OpenObserveSettings {
        OpenObserveSettings {
            url: url.to_owned(),
            stream: "dekopon".to_owned(),
            authorization: "Basic dGVzdA==".to_owned(),
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn the_search_url_is_the_organization_base_plus_the_logs_search_path() {
        let client =
            OpenObserveClient::new(settings("http://127.0.0.1:5080/api/default/")).expect("valid");
        assert_eq!(
            client.search_url,
            "http://127.0.0.1:5080/api/default/_search?type=logs"
        );
        assert_eq!(
            client
                .trace_sql("4bf92f3577b34da6a3ce929d0e0e4736")
                .expect("valid trace"),
            "SELECT * FROM \"dekopon\" WHERE trace_id = '4bf92f3577b34da6a3ce929d0e0e4736'"
        );
        assert!(
            client
                .accounting_sql()
                .contains("audit_event = 'accounting.model.turn'")
        );
    }

    /// The trace identifier is interpolated into SQL, so its alphabet is the whole defence.
    #[test]
    fn a_trace_identifier_outside_its_alphabet_is_refused() {
        let client =
            OpenObserveClient::new(settings("http://127.0.0.1:5080/api/default")).expect("valid");
        for bad in ["", "abc' OR 1=1 --", "a b", &"x".repeat(129)] {
            assert!(
                matches!(client.trace_sql(bad), Err(ObserveError::Configuration(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn settings_are_validated_before_anything_is_sent() {
        assert!(OpenObserveClient::new(settings("  ")).is_err());
        assert!(OpenObserveClient::new(settings("http://user:pw@host/api/default")).is_err());
        assert!(OpenObserveClient::new(settings("http://host/api/default?x=1")).is_err());
        let mut bad_stream = settings("http://host/api/default");
        bad_stream.stream = "dekopon-logs".to_owned();
        assert!(OpenObserveClient::new(bad_stream).is_err());
        let mut blank_auth = settings("http://host/api/default");
        blank_auth.authorization = " ".to_owned();
        assert!(OpenObserveClient::new(blank_auth).is_err());
        let mut zero = settings("http://host/api/default");
        zero.timeout = Duration::ZERO;
        assert!(OpenObserveClient::new(zero).is_err());
    }
}
