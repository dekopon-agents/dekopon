//! Rust guest facade for the buffered `dekopon:http@1.0.0` component interface.
//!
//! This crate contains no HTTP transport. [`send`] calls a host import that only a separately
//! authorized broker is expected to implement.

#![forbid(unsafe_code)]

use std::{error::Error, fmt};

/// The imported HTTP WIT contract used by the generated guest bindings.
pub const HTTP_WIT: &str = include_str!("../wit/deps/http.wit");

/// Common HTTP method tokens.
pub mod method {
    /// CONNECT.
    pub const CONNECT: &str = "CONNECT";
    /// DELETE.
    pub const DELETE: &str = "DELETE";
    /// GET.
    pub const GET: &str = "GET";
    /// HEAD.
    pub const HEAD: &str = "HEAD";
    /// OPTIONS.
    pub const OPTIONS: &str = "OPTIONS";
    /// PATCH.
    pub const PATCH: &str = "PATCH";
    /// POST.
    pub const POST: &str = "POST";
    /// PUT.
    pub const PUT: &str = "PUT";
    /// TRACE.
    pub const TRACE: &str = "TRACE";
}

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "http-client",
        generate_all,
    });
}

/// One ordered HTTP header with an opaque byte value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    /// Case-insensitive HTTP field name.
    pub name: String,
    /// Field value bytes.
    pub value: Vec<u8>,
}

impl Header {
    /// Creates and validates one header.
    pub fn new(name: impl Into<String>, value: impl Into<Vec<u8>>) -> Result<Self, BuildError> {
        let name = name.into();
        if !is_token(&name) {
            return Err(BuildError::InvalidHeaderName(name));
        }
        let value = value.into();
        if !is_field_value(&value) {
            return Err(BuildError::InvalidHeaderValue(name));
        }
        Ok(Self { name, value })
    }

    /// Creates one header from a UTF-8 value.
    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Result<Self, BuildError> {
        Self::new(name, value.into().into_bytes())
    }
}

/// A complete buffered HTTP request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    /// Any valid standard or extension HTTP method token.
    pub method: String,
    /// Absolute request URI. The broker performs authoritative URI and policy validation.
    pub uri: String,
    /// Ordered headers; duplicate field names are preserved.
    pub headers: Vec<Header>,
    /// Complete request body.
    pub body: Vec<u8>,
}

impl Request {
    /// Creates a request without headers or a body.
    pub fn new(method: impl Into<String>, uri: impl Into<String>) -> Result<Self, BuildError> {
        let method = method.into();
        if !is_token(&method) {
            return Err(BuildError::InvalidMethod(method));
        }
        let uri = uri.into();
        if uri.is_empty() {
            return Err(BuildError::EmptyUri);
        }
        Ok(Self {
            method,
            uri,
            headers: Vec::new(),
            body: Vec::new(),
        })
    }

    /// Appends one header without coalescing duplicate names.
    #[must_use]
    pub fn with_header(mut self, header: Header) -> Self {
        self.headers.push(header);
        self
    }

    /// Replaces the complete buffered request body.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
}

/// A complete buffered HTTP response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Ordered response headers; duplicate field names are preserved.
    pub headers: Vec<Header>,
    /// Complete response body.
    pub body: Vec<u8>,
}

/// Stable failure classes returned by the broker HTTP host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpErrorCode {
    /// The method was not an HTTP token.
    InvalidMethod,
    /// The URI was invalid or unsupported.
    InvalidUri,
    /// A header was malformed or guest-controlled when broker ownership was required.
    InvalidHeader,
    /// The encoded request exceeded an authorized bound.
    RequestTooLarge,
    /// Policy denied the request.
    Denied,
    /// The invocation exhausted its host-call budget.
    HostCallLimit,
    /// Destination resolution failed or was rejected.
    Dns,
    /// A connection could not be established.
    Connect,
    /// TLS negotiation or validation failed.
    Tls,
    /// The bounded operation timed out.
    Timeout,
    /// The remote peer violated HTTP protocol expectations.
    Protocol,
    /// The response exceeded an authorized bound.
    ResponseTooLarge,
    /// The broker encountered an internal failure.
    Internal,
}

impl HttpErrorCode {
    /// Returns the WIT enum name for this class.
    ///
    /// These are the stable machine-readable identifiers of the `dekopon:http@1.0.0` contract and
    /// the spelling `docs/broker-http.md` uses. The Rust variant name is not part of any contract,
    /// so anything a provider stringifies must use this instead.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidMethod => "invalid-method",
            Self::InvalidUri => "invalid-uri",
            Self::InvalidHeader => "invalid-header",
            Self::RequestTooLarge => "request-too-large",
            Self::Denied => "denied",
            Self::HostCallLimit => "host-call-limit",
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Timeout => "timeout",
            Self::Protocol => "protocol",
            Self::ResponseTooLarge => "response-too-large",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for HttpErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A bounded failure returned across the HTTP component boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpError {
    /// Stable machine-readable class.
    pub code: HttpErrorCode,
    /// Bounded provider-safe detail.
    pub message: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl Error for HttpError {}

/// A request or header could not be represented as HTTP.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildError {
    /// Method was empty or contained a non-token byte.
    InvalidMethod(String),
    /// URI was empty. Complete URI validation remains host-owned.
    EmptyUri,
    /// Header name was empty or contained a non-token byte.
    InvalidHeaderName(String),
    /// Header value contained a prohibited control byte.
    InvalidHeaderValue(String),
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMethod(method) => write!(formatter, "invalid HTTP method {method:?}"),
            Self::EmptyUri => formatter.write_str("HTTP URI must not be empty"),
            Self::InvalidHeaderName(name) => write!(formatter, "invalid HTTP header name {name:?}"),
            Self::InvalidHeaderValue(name) => {
                write!(formatter, "HTTP header {name:?} contains a prohibited byte")
            }
        }
    }
}

impl Error for BuildError {}

/// Sends one request through the broker-provided component import.
///
/// This function does not confer authority. The host validates the request against the current
/// authorized invocation and may return [`HttpErrorCode::Denied`].
pub fn send(request: Request) -> Result<Response, HttpError> {
    let request = bindings::dekopon::http::client::Request {
        method: request.method,
        uri: request.uri,
        headers: request
            .headers
            .into_iter()
            .map(|header| bindings::dekopon::http::client::Header {
                name: header.name,
                value: header.value,
            })
            .collect(),
        body: request.body,
    };

    bindings::dekopon::http::client::send(&request)
        .map(|response| Response {
            status: response.status,
            headers: response
                .headers
                .into_iter()
                .map(|header| Header {
                    name: header.name,
                    value: header.value,
                })
                .collect(),
            body: response.body,
        })
        .map_err(|error| HttpError {
            code: map_error_code(error.code),
            message: error.message,
        })
}

fn map_error_code(code: bindings::dekopon::http::client::ErrorCode) -> HttpErrorCode {
    use bindings::dekopon::http::client::ErrorCode as Wit;
    match code {
        Wit::InvalidMethod => HttpErrorCode::InvalidMethod,
        Wit::InvalidUri => HttpErrorCode::InvalidUri,
        Wit::InvalidHeader => HttpErrorCode::InvalidHeader,
        Wit::RequestTooLarge => HttpErrorCode::RequestTooLarge,
        Wit::Denied => HttpErrorCode::Denied,
        Wit::HostCallLimit => HttpErrorCode::HostCallLimit,
        Wit::Dns => HttpErrorCode::Dns,
        Wit::Connect => HttpErrorCode::Connect,
        Wit::Tls => HttpErrorCode::Tls,
        Wit::Timeout => HttpErrorCode::Timeout,
        Wit::Protocol => HttpErrorCode::Protocol,
        Wit::ResponseTooLarge => HttpErrorCode::ResponseTooLarge,
        Wit::Internal => HttpErrorCode::Internal,
    }
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn is_field_value(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| *byte == b'\t' || (*byte >= 0x20 && *byte != 0x7f))
}

#[cfg(test)]
mod tests {
    use super::{HTTP_WIT, Header, HttpError, HttpErrorCode, Request, method};

    const ALL_CODES: [HttpErrorCode; 13] = [
        HttpErrorCode::InvalidMethod,
        HttpErrorCode::InvalidUri,
        HttpErrorCode::InvalidHeader,
        HttpErrorCode::RequestTooLarge,
        HttpErrorCode::Denied,
        HttpErrorCode::HostCallLimit,
        HttpErrorCode::Dns,
        HttpErrorCode::Connect,
        HttpErrorCode::Tls,
        HttpErrorCode::Timeout,
        HttpErrorCode::Protocol,
        HttpErrorCode::ResponseTooLarge,
        HttpErrorCode::Internal,
    ];

    #[test]
    fn accepts_standard_and_extension_methods() {
        assert!(Request::new(method::PATCH, "https://example.test/items/1").is_ok());
        assert!(Request::new("PROPFIND", "https://example.test/").is_ok());
    }

    #[test]
    fn rejects_non_token_methods_and_header_names() {
        assert!(Request::new("BAD METHOD", "https://example.test/").is_err());
        assert!(Header::text("bad header", "value").is_err());
    }

    #[test]
    fn rejects_header_line_injection() {
        assert!(Header::text("x-example", "safe\r\ninjected: value").is_err());
    }

    /// The rendered code is the WIT enum name, read out of the contract itself.
    ///
    /// A provider that stringifies an error emits an identifier that flows into `ProviderError`
    /// messages, `InvocationResult`, and payload-carrying telemetry. Rendering the Rust variant
    /// spelling there would match neither `dekopon:http@1.0.0` nor `docs/broker-http.md`.
    #[test]
    fn error_codes_render_the_wit_names() {
        let block = HTTP_WIT
            .split_once("enum error-code {")
            .expect("the contract declares error-code")
            .1
            .split_once('}')
            .expect("the enum is closed")
            .0;
        let declared = block
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_suffix(','))
            .collect::<Vec<_>>();

        assert_eq!(
            declared,
            ALL_CODES
                .iter()
                .map(|code| code.as_str())
                .collect::<Vec<_>>()
        );

        for code in ALL_CODES {
            assert_eq!(code.to_string(), code.as_str());
        }

        let error = HttpError {
            code: HttpErrorCode::ResponseTooLarge,
            message: "body exceeded the authorized bound".to_owned(),
        };
        assert_eq!(
            error.to_string(),
            "response-too-large: body exceeded the authorized bound"
        );
    }
}
