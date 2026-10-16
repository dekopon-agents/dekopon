use std::{io, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Bytes, to_bytes},
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use hmac::{Hmac, KeyInit as _, Mac as _};
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use sha2::Sha256;
use tokio::sync::Semaphore;

use super::TransportError;

pub(super) const MAX_WEBHOOK_BODY_BYTES: usize = 256 * 1024;
pub(super) const MAX_WEBHOOK_HEADERS: usize = 32;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_CONNECTION_BUFFER_BYTES: usize = 16 * 1024;
pub(super) const MAX_WEBHOOK_CONCURRENCY: usize = 16;
pub(super) const WEBHOOK_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn listen(
    bind: std::net::SocketAddr,
    router: Router,
    name: String,
    server: &mut tokio::task::JoinSet<()>,
) -> Result<(), TransportError> {
    if !server.is_empty() {
        return Err(TransportError::Response);
    }
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(TransportError::Io)?;
    server.spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        let connection_limit = Arc::new(Semaphore::new(MAX_WEBHOOK_CONCURRENCY));
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let stream = match accepted {
                        Ok((stream, _peer)) => stream,
                        Err(error) => match classify_accept(&error) {
                            AcceptFailure::Connection => {
                                tracing::debug!(
                                    event = "gateway_whatsapp_accept_failed",
                                    transport = %name,
                                    kind = "connection",
                                    error = %error,
                                );
                                continue;
                            }
                            AcceptFailure::Exhausted => {
                                tracing::warn!(
                                    event = "gateway_whatsapp_accept_failed",
                                    transport = %name,
                                    kind = "exhausted",
                                    error = %error,
                                );
                                return;
                            }
                            AcceptFailure::Fatal => {
                                tracing::error!(
                                    event = "gateway_whatsapp_listener_stopped",
                                    transport = %name,
                                    error = %error,
                                );
                                return;
                            }
                        },
                    };
                    let Ok(connection_permit) = Arc::clone(&connection_limit).try_acquire_owned() else {
                        continue;
                    };
                    let service = TowerToHyperService::new(router.clone());
                    connections.spawn(async move {
                        let _connection_permit = connection_permit;
                        let mut builder = hyper::server::conn::http1::Builder::new();
                        builder
                            .max_headers(MAX_WEBHOOK_HEADERS)
                            .max_buf_size(MAX_CONNECTION_BUFFER_BYTES);
                        let connection = builder.serve_connection(TokioIo::new(stream), service);
                        // Header parsing is included in this deadline so a slow client cannot hold a socket indefinitely.
                        #[allow(
                            clippy::let_underscore_must_use,
                            reason = "the outcome is that one untrusted client's \
                                      connection ended, by deadline or by hanging up; the \
                                      router already recorded whatever it answered"
                        )]
                        let _ = tokio::time::timeout(WEBHOOK_REQUEST_TIMEOUT, connection).await;
                    });
                }
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    });
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AcceptFailure {
    Connection,
    Exhausted,
    Fatal,
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "std::io::ErrorKind is non-exhaustive; an unknown kind is still classified by its OS error"
)]
fn classify_accept(error: &io::Error) -> AcceptFailure {
    match error.kind() {
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::Interrupted
        | io::ErrorKind::TimedOut
        | io::ErrorKind::WouldBlock => AcceptFailure::Connection,
        io::ErrorKind::OutOfMemory => AcceptFailure::Exhausted,
        _ => match error.raw_os_error() {
            Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM) => {
                AcceptFailure::Exhausted
            }
            _ => AcceptFailure::Fatal,
        },
    }
}

pub(super) enum VerifyFailure {
    Headers,
    Unsigned,
    Oversize,
    Signature,
}

pub(super) async fn verified_body(request: Request, secret: &[u8]) -> Result<Bytes, VerifyFailure> {
    if !headers_bounded(request.headers()) {
        return Err(VerifyFailure::Headers);
    }
    let signature = exact_signature(request.headers()).ok_or(VerifyFailure::Unsigned)?;
    if request
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_WEBHOOK_BODY_BYTES)
    {
        return Err(VerifyFailure::Oversize);
    }
    let body = to_bytes(request.into_body(), MAX_WEBHOOK_BODY_BYTES)
        .await
        .map_err(|_error| VerifyFailure::Oversize)?;
    if !valid_hmac_sha256(secret, &body, &signature) {
        return Err(VerifyFailure::Signature);
    }
    Ok(body)
}

pub(super) fn headers_bounded(headers: &HeaderMap) -> bool {
    let mut count = 0_usize;
    let mut bytes = 0_usize;
    for (name, value) in headers {
        count = count.saturating_add(1);
        bytes = bytes
            .saturating_add(name.as_str().len())
            .saturating_add(value.as_bytes().len());
        if count > MAX_WEBHOOK_HEADERS || bytes > MAX_HEADER_BYTES {
            return false;
        }
    }
    true
}

fn exact_signature(headers: &HeaderMap) -> Option<[u8; 32]> {
    let values: Vec<_> = headers.get_all("x-hub-signature-256").iter().collect();
    if values.len() != 1 {
        return None;
    }
    let value = values[0].to_str().ok()?.strip_prefix("sha256=")?;
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        digest[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(digest)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn hmac_sha256(key: &[u8], body: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of every length");
    mac.update(body);
    mac.finalize().into_bytes().into()
}

fn valid_hmac_sha256(key: &[u8], body: &[u8], signature: &[u8]) -> bool {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of every length");
    mac.update(body);
    mac.verify_slice(signature).is_ok()
}

pub(super) fn content_free(status: StatusCode) -> Response {
    text_response(status, String::new())
}

pub(super) fn text_response(status: StatusCode, body: String) -> Response {
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    #[test]
    fn hmac_matches_a_known_vector_and_uses_exact_raw_bytes() {
        let compact = br#"{"object":"whatsapp_business_account"}"#;
        let spaced = br#"{ "object": "whatsapp_business_account" }"#;
        assert_ne!(
            hmac_sha256(b"secret", compact),
            hmac_sha256(b"secret", spaced)
        );
        assert!(valid_hmac_sha256(
            b"secret",
            compact,
            &hmac_sha256(b"secret", compact)
        ));
        let known = hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog");
        let known: String = known.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            known,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn accept_failures_distinguish_peer_errors_resource_exhaustion_and_listener_loss() {
        for kind in [
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::Interrupted,
            io::ErrorKind::TimedOut,
            io::ErrorKind::WouldBlock,
        ] {
            assert_eq!(
                classify_accept(&io::Error::from(kind)),
                AcceptFailure::Connection
            );
        }
        for code in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert_eq!(
                classify_accept(&io::Error::from_raw_os_error(code)),
                AcceptFailure::Exhausted
            );
        }
        assert_eq!(
            classify_accept(&io::Error::from_raw_os_error(libc::EBADF)),
            AcceptFailure::Fatal
        );
    }

    #[tokio::test]
    async fn bounded_headers_and_body_refuse_before_hmac() {
        let mut builder = Request::builder();
        for index in 0..33 {
            builder = builder.header(format!("x-test-{index}"), "v");
        }
        let request = builder.body(Body::empty()).expect("request");
        assert!(matches!(
            verified_body(request, b"secret").await,
            Err(VerifyFailure::Headers)
        ));
        for chunked in [false, true] {
            let body = vec![0_u8; MAX_WEBHOOK_BODY_BYTES + 1];
            let mut builder = Request::builder().header(
                "x-hub-signature-256",
                "sha256=0000000000000000000000000000000000000000000000000000000000000000",
            );
            if !chunked {
                builder = builder.header("content-length", body.len());
            }
            let request = builder.body(Body::from(body)).expect("request");
            assert!(matches!(
                verified_body(request, b"secret").await,
                Err(VerifyFailure::Oversize)
            ));
        }
    }
}
