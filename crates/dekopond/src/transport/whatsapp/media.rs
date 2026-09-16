//! Bounded WhatsApp media courier. No webhook or model content chooses a credential sink.

use std::{net::IpAddr, time::Instant};

use tracing::Instrument as _;

use crate::transport::hydration::HydratedImage;
use dekopon_agent::attachment::validate_png;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use super::*;

/// Conservative decimal interpretation of Meta's image-specific 5 MB ceiling.
pub(super) const MAX_IMAGE_BYTES: usize = 5_000_000;
pub(super) const MAX_CAPTION_CHARS: usize = 1024;
const DOWNLOAD_HOST: &str = "lookaside.fbsbx.com";
const DOWNLOAD_PATH: &str = "/whatsapp_business/attachments/";

fn media_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 64 && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub(super) fn inbound_image(image: &Value) -> Option<PendingAsset> {
    let id = image.get("id")?.as_str()?;
    let mime = image.get("mime_type")?.as_str()?;
    let extension = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        _ => return None,
    };
    if !media_id(id) {
        return None;
    }
    Some(PendingAsset {
        name: format!("photo.{extension}"),
        mime: mime.to_owned(),
        size: 0, // Webhooks do not promise a length; the lazy reader enforces it.
        source: Some(AssetSourceRef::WhatsApp {
            media_id: id.to_owned(),
            mime: mime.to_owned(),
        }),
    })
}

/// Fixed local reason only, never service text, an expiring URL, or a credential.
pub(super) fn failure(reason: &str) -> TransportError {
    tracing::warn!(event = "gateway_whatsapp_media_refused", reason);
    TransportError::Service {
        code: reason.to_owned(),
    }
}

fn request_failed(error: reqwest::Error) -> TransportError {
    let cause = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_body() {
        "body"
    } else {
        "request"
    };
    tracing::warn!(event = "gateway_whatsapp_media_refused", reason = cause);
    TransportError::Request(Box::new(error.without_url()))
}

async fn read_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, TransportError> {
    if !response.status().is_success() {
        return Err(failure(&format!("http-{}", response.status().as_u16())));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(failure("response-too-large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_failed)? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(failure("response-too-large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn json_response(response: reqwest::Response) -> Result<Value, TransportError> {
    let bytes = read_body(response, MAX_GRAPH_RESPONSE_BYTES).await?;
    serde_json::from_slice(&bytes).map_err(|source| {
        tracing::warn!(
            event = "gateway_whatsapp_media_refused",
            reason = "json",
            line = source.line(),
            column = source.column()
        );
        TransportError::MalformedResponse(source)
    })
}

impl WhatsappDriver {
    fn download_url(&self, value: &str) -> Result<reqwest::Url, TransportError> {
        let url = reqwest::Url::parse(value).map_err(|source| {
            tracing::warn!(event = "gateway_whatsapp_media_refused", reason = "url-parse", cause = %source);
            TransportError::Response
        })?;
        let trusted = url.scheme() == "https"
            && url.host_str() == Some(DOWNLOAD_HOST)
            && url.port_or_known_default() == Some(443);
        // Only tests may replace the exact origin; runtime config pins Graph and cannot add a CDN.
        #[cfg(test)]
        let trusted = trusted
            || reqwest::Url::parse(&self.endpoint).is_ok_and(|origin| {
                origin.scheme() == "http"
                    && origin
                        .host_str()
                        .is_some_and(|host| host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()))
                    && origin.origin() == url.origin()
            });
        if !trusted
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.path() != DOWNLOAD_PATH
        {
            return Err(failure("media-destination"));
        }
        Ok(url)
    }

    pub(super) fn reply_failure(&self, error: TransportError, accepted: usize) -> TransportError {
        if accepted == 0 {
            return error;
        }
        tracing::warn!(event = "gateway_whatsapp_reply_partial", transport = %self.transport,
            category = error.category(), delivered = accepted);
        TransportError::PartialDelivery
    }

    pub(super) async fn send_image(
        &self,
        recipient: &str,
        caption: Option<&str>,
        image: HydratedImage,
    ) -> Result<(), TransportError> {
        let bytes = image.bytes.len();
        let upload = tracing::info_span!(
            "whatsapp.image_upload",
            bytes,
            duration_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            reason = tracing::field::Empty
        );
        let started = Instant::now();
        let result = async {
            let filename = image.filename;
            let part = reqwest::multipart::Part::bytes(image.bytes)
                .file_name(filename)
                .mime_str("image/png")
                .map_err(request_failed)?;
            let form = reqwest::multipart::Form::new()
                .text("messaging_product", "whatsapp")
                .part("file", part);
            let response = self
                .http
                .post(format!(
                    "{}/{}/{}/media",
                    self.endpoint, self.version, self.phone_number_id
                ))
                .bearer_auth(self.access_token.expose())
                .multipart(form)
                .send()
                .await
                .map_err(request_failed)?;
            let uploaded = json_response(response).await?;
            uploaded
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| media_id(id))
                .map(str::to_owned)
                .ok_or_else(|| failure("upload-id"))
        }
        .instrument(upload.clone())
        .await;
        record_media_result(&upload, started, &result);
        let id = result?;
        let send = tracing::info_span!(
            "whatsapp.image_send",
            bytes,
            duration_ms = tracing::field::Empty,
            outcome = tracing::field::Empty,
            reason = tracing::field::Empty
        );
        let started = Instant::now();
        let result = async {
            let mut payload = json!({
                "messaging_product": "whatsapp", "recipient_type": "individual", "to": recipient,
                "type": "image", "image": {"id": id}
            });
            if let Some(caption) = caption {
                payload["image"]["caption"] = json!(caption);
            }
            let response = self
                .http
                .post(self.messages_url())
                .bearer_auth(self.access_token.expose())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(payload.to_string())
                .send()
                .await
                .map_err(request_failed)?;
            let sent = json_response(response).await?;
            if sent.get("messaging_product").and_then(Value::as_str) != Some("whatsapp")
                || !sent
                    .pointer("/messages/0/id")
                    .and_then(Value::as_str)
                    .is_some_and(canonical_whatsapp_message_id)
            {
                return Err(failure("message-acceptance"));
            }
            Ok(())
        }
        .instrument(send.clone())
        .await;
        record_media_result(&send, started, &result);
        result
    }
}

fn record_media_result<T>(
    span: &tracing::Span,
    started: Instant,
    result: &Result<T, TransportError>,
) {
    span.record("duration_ms", started.elapsed().as_millis() as u64);
    span.record(
        "outcome",
        if result.is_ok() { "accepted" } else { "failed" },
    );
    if let Err(error) = result {
        span.record("reason", error.category());
    }
}

impl AssetFetcher for WhatsappDriver {
    fn fetch(
        &self,
        source: &AssetSourceRef,
        max_bytes: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, TransportError>> {
        let AssetSourceRef::WhatsApp { media_id: id, mime } = source else {
            return Box::pin(async { Err(failure("asset-source")) });
        };
        let (id, mime) = (id.clone(), mime.clone());
        Box::pin(async move {
            if !media_id(&id) || !matches!(mime.as_str(), "image/png" | "image/jpeg") {
                return Err(failure("asset-metadata"));
            }
            let limit = max_bytes.min(MAX_IMAGE_BYTES as u64) as usize;
            let response = self
                .http
                .get(format!("{}/{}/{}", self.endpoint, self.version, id))
                .query(&[("phone_number_id", &self.phone_number_id)])
                .bearer_auth(self.access_token.expose())
                .send()
                .await
                .map_err(request_failed)?;
            let metadata = json_response(response).await?;
            if metadata.get("id").and_then(Value::as_str) != Some(&id)
                || metadata.get("mime_type").and_then(Value::as_str) != Some(&mime)
            {
                return Err(failure("asset-metadata"));
            }
            let size = metadata
                .get("file_size")
                .and_then(|size| size.as_u64().or_else(|| size.as_str()?.parse::<u64>().ok()))
                .ok_or_else(|| failure("media-size"))?;
            if size == 0 || size > limit as u64 {
                return Err(failure("image-too-large"));
            }
            let url = self.download_url(
                metadata
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| failure("media-url"))?,
            )?;
            let response = self
                .http
                .get(url)
                .bearer_auth(self.access_token.expose())
                .send()
                .await
                .map_err(request_failed)?;
            let bytes = read_body(response, limit).await?;
            if bytes.len() as u64 != size {
                return Err(failure("media-size-mismatch"));
            }
            // Match the existing courier's signature-level validation, not full image decoding.
            let valid = match mime.as_str() {
                "image/png" => validate_png(&bytes).is_ok(),
                "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
                _ => false,
            };
            if !valid {
                return Err(failure("image-signature"));
            }
            Ok(bytes)
        })
    }
}

/// Resolver results are the addresses reqwest actually connects to, not a preflight lookup that
/// it repeats later. Only the two fixed WhatsApp services may resolve; no ambient proxy is used.
pub(super) struct WhatsappResolver;
impl Resolve for WhatsappResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            if !matches!(name.as_str(), "graph.facebook.com" | DOWNLOAD_HOST) {
                return Err(io::Error::other("WhatsApp DNS host refused").into());
            }
            let addresses: Vec<_> = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::lookup_host((name.as_str(), 443)),
            )
            .await
            .map_err(|source| io::Error::new(io::ErrorKind::TimedOut, source))??
            .take(33)
            .collect();
            if addresses.is_empty()
                || addresses.len() > 32
                || addresses
                    .iter()
                    .any(|address| !public_address(address.ip()))
            {
                return Err(
                    io::Error::other("WhatsApp DNS non-public or excessive addresses").into(),
                );
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

/// Conservative direct-public-address policy for this transport's two credential sinks.
fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192
                    && ((b == 0 && (c == 0 || c == 2)) || (b == 88 && c == 99) || b == 168))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && ((s[1] & 0xfe00) == 0 || s[1] == 0xdb8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && (s[1] & 0xf000) == 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dns_addresses_refuse_private_transition_and_documentation_ranges() {
        for value in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "224.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2002:7f00:1::1",
        ] {
            assert!(!public_address(value.parse().expect("IP")), "{value}");
        }
        for value in ["8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_address(value.parse().expect("IP")), "{value}");
        }
    }
}
