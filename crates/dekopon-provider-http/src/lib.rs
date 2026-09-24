#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::{error::Error, fmt};

use dekopon_provider_sdk::asset::{Encoding, Handle};

pub const HTTP_WIT: &str = include_str!("../wit/deps/http.wit");

pub mod method {
    pub const CONNECT: &str = "CONNECT";
    pub const DELETE: &str = "DELETE";
    pub const GET: &str = "GET";
    pub const HEAD: &str = "HEAD";
    pub const OPTIONS: &str = "OPTIONS";
    pub const PATCH: &str = "PATCH";
    pub const POST: &str = "POST";
    pub const PUT: &str = "PUT";
    pub const TRACE: &str = "TRACE";
}

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "http-client",
        with: {
            "dekopon:asset/asset@0.1.0": dekopon_provider_sdk::asset::bindings::dekopon::asset::asset,
        },
        generate_all,
    });
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    pub name: String,
    pub value: Vec<u8>,
}

impl Header {
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

    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Result<Self, BuildError> {
        Self::new(name, value.into().into_bytes())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    pub method: String,
    /// Absolute request URI. The broker performs authoritative URI and policy validation.
    pub uri: String,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
}

impl Request {
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

    #[must_use]
    pub fn with_header(mut self, header: Header) -> Self {
        self.headers.push(header);
        self
    }

    #[must_use]
    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum Part<'a> {
    Literal(Vec<u8>),
    Asset {
        handle: &'a Handle,
        encoding: Encoding,
    },
}

impl<'a> Part<'a> {
    pub fn literal(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Literal(bytes.into())
    }

    pub fn asset(handle: &'a Handle, encoding: Encoding) -> Self {
        Self::Asset { handle, encoding }
    }
}

#[derive(Debug)]
pub struct StreamedRequest<'a> {
    pub method: String,
    pub uri: String,
    pub headers: Vec<Header>,
    pub body: Vec<Part<'a>>,
}

impl<'a> StreamedRequest<'a> {
    pub fn new(method: impl Into<String>, uri: impl Into<String>) -> Result<Self, BuildError> {
        let request = Request::new(method, uri)?;
        Ok(Self {
            method: request.method,
            uri: request.uri,
            headers: request.headers,
            body: Vec::new(),
        })
    }

    #[must_use]
    pub fn with_header(mut self, header: Header) -> Self {
        self.headers.push(header);
        self
    }

    #[must_use]
    pub fn with_body(mut self, body: Vec<Part<'a>>) -> Self {
        self.body = body;
        self
    }
}

#[derive(Debug)]
pub struct StreamedResponse {
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: Handle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpErrorCode {
    InvalidMethod,
    InvalidUri,
    InvalidHeader,
    RequestTooLarge,
    Denied,
    HostCallLimit,
    Dns,
    Connect,
    Tls,
    Timeout,
    Protocol,
    ResponseTooLarge,
    Internal,
}

impl HttpErrorCode {
    /// Returns the WIT contract's stable error-code name rather than the Rust variant name, since a
    /// provider's stringified error must match the wire contract, not this crate's naming.
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpError {
    pub code: HttpErrorCode,
    pub message: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl Error for HttpError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildError {
    InvalidMethod(String),
    EmptyUri,
    InvalidHeaderName(String),
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

/// Calling this confers no authority; the host validates the request against the current authorized
/// invocation and may refuse it with Denied.
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

/// Grants no authority; the broker enforces HTTP and asset bounds and must have its asset directory
/// configured before it will spool a response.
pub fn stream(request: StreamedRequest<'_>) -> Result<StreamedResponse, HttpError> {
    use bindings::dekopon::http::client as wit;
    let request = wit::StreamedRequest {
        method: request.method,
        uri: request.uri,
        headers: request
            .headers
            .into_iter()
            .map(|header| wit::Header {
                name: header.name,
                value: header.value,
            })
            .collect(),
        body: request
            .body
            .into_iter()
            .map(|part| match part {
                Part::Literal(bytes) => wit::Part::Literal(bytes),
                Part::Asset { handle, encoding } => wit::Part::Asset(wit::AssetPart {
                    handle: handle.as_inner(),
                    encoding,
                }),
            })
            .collect(),
    };
    wit::stream(&request)
        .map(|response| StreamedResponse {
            status: response.status,
            headers: response
                .headers
                .into_iter()
                .map(|header| Header {
                    name: header.name,
                    value: header.value,
                })
                .collect(),
            body: Handle::from_inner(response.body),
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
    use super::{
        HTTP_WIT, Header, HttpError, HttpErrorCode, Part, Request, StreamedRequest, method,
    };

    #[test]
    fn streamed_builder_validates_and_preserves_part_and_header_order() {
        assert!(StreamedRequest::new("BAD METHOD", "https://example.test/").is_err());
        assert!(StreamedRequest::new(method::POST, "").is_err());
        let request = StreamedRequest::new(method::POST, "https://example.test/")
            .unwrap()
            .with_header(Header::text("x-example", "first").unwrap())
            .with_header(Header::text("x-example", "second").unwrap())
            .with_body(vec![
                Part::literal(b"head".to_vec()),
                Part::literal(b"tail".to_vec()),
            ]);
        assert_eq!(request.headers[0].value, b"first");
        assert_eq!(request.headers[1].value, b"second");
        assert!(matches!(&request.body[0], Part::Literal(bytes) if bytes == b"head"));
        assert!(matches!(&request.body[1], Part::Literal(bytes) if bytes == b"tail"));
    }

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
