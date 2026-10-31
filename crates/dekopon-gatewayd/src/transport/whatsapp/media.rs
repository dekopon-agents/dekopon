use std::{net::IpAddr, time::Instant};

use tracing::Instrument as _;

use crate::transport::{asset_buffer, floor_boundary, hydration::HydratedImage, reserve_for_chunk};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use super::*;

pub(crate) const MAX_IMAGE_BYTES: usize = 5_000_000;
pub(super) const MAX_CAPTION_CHARS: usize = 1024;
const MAX_DOCUMENT_NAME_BYTES: usize = 128;
const PDF: &str = "application/pdf";
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
        size: None,
        source: Some(AssetSourceRef::WhatsApp {
            media_id: id.to_owned(),
            mime: mime.to_owned(),
        }),
    })
}

pub(super) fn inbound_document(
    document: &Value,
) -> (&str, Vec<PendingAsset>, Option<InputRefusal>) {
    let caption = match document.get("caption") {
        None => Some(""),
        Some(Value::String(caption)) => Some(caption.as_str()),
        Some(_) => None,
    };
    match (caption, pdf(document)) {
        (Some(caption), Some(asset)) => (caption, vec![asset], None),
        (caption, _) => (
            caption.unwrap_or_default(),
            Vec::new(),
            Some(InputRefusal::UnsupportedDocument),
        ),
    }
}

fn pdf(document: &Value) -> Option<PendingAsset> {
    let id = document.get("id")?.as_str()?;
    if document.get("mime_type")?.as_str()? != PDF || !media_id(id) {
        return None;
    }
    Some(PendingAsset {
        name: document_name(document.get("filename").and_then(Value::as_str)),
        mime: PDF.to_owned(),
        size: None,
        source: Some(AssetSourceRef::WhatsApp {
            media_id: id.to_owned(),
            mime: PDF.to_owned(),
        }),
    })
}

fn document_name(filename: Option<&str>) -> String {
    let base = filename
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let name: String = base
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let name = name.trim();
    let name = &name[..floor_boundary(name, MAX_DOCUMENT_NAME_BYTES)];
    if name.is_empty() || name.chars().all(|character| character == '.') {
        "document.pdf".to_owned()
    } else {
        name.to_owned()
    }
}

#[derive(Clone, Copy)]
enum Media {
    Png,
    Jpeg,
    Pdf,
}

impl Media {
    fn of(mime: &str) -> Option<Self> {
        match mime {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            PDF => Some(Self::Pdf),
            _ => None,
        }
    }

    fn limit(self, max_bytes: u64) -> u64 {
        match self {
            Self::Png | Self::Jpeg => max_bytes.min(MAX_IMAGE_BYTES as u64),
            Self::Pdf => max_bytes,
        }
    }

    fn oversized(self, size: u64) -> TransportError {
        match self {
            Self::Png | Self::Jpeg => failure("image-too-large"),
            Self::Pdf if size == 0 => failure("media-size"),
            Self::Pdf => {
                refused("document-too-large");
                TransportError::AssetTooLarge { size }
            }
        }
    }

    fn body_failure(self, failure: BodyFailure) -> TransportError {
        match (self, failure) {
            (Self::Pdf, BodyFailure::TooLarge { observed }) => self.oversized(observed),
            (Self::Pdf, BodyFailure::Status(status)) => {
                refused(&format!("http-{status}"));
                TransportError::AssetRefused {
                    cause: match status {
                        404 | 410 => MediaRefusal::Gone,
                        _ => MediaRefusal::Download { status },
                    },
                }
            }
            (Self::Png | Self::Jpeg | Self::Pdf, failure) => failure.into_service(),
        }
    }

    fn signature_failure(self) -> TransportError {
        match self {
            Self::Png | Self::Jpeg => failure("image-signature"),
            Self::Pdf => {
                refused("document-signature");
                TransportError::AssetRefused {
                    cause: MediaRefusal::Signature,
                }
            }
        }
    }
}

fn refused(reason: &str) {
    tracing::warn!(
        event = "gateway_whatsapp_media_refused",
        reason,
        "gateway.whatsapp.media.refused"
    );
}

pub(super) fn failure(reason: &str) -> TransportError {
    refused(reason);
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
    tracing::warn!(
        event = "gateway_whatsapp_media_refused",
        reason = cause,
        "gateway.whatsapp.media.refused"
    );
    TransportError::Request(Box::new(error.without_url()))
}

enum BodyFailure {
    Status(u16),
    TooLarge { observed: u64 },
    Transport(TransportError),
}

impl BodyFailure {
    fn into_service(self) -> TransportError {
        match self {
            Self::Status(status) => failure(&format!("http-{status}")),
            Self::TooLarge { .. } => failure("response-too-large"),
            Self::Transport(error) => error,
        }
    }
}

async fn bounded_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, BodyFailure> {
    if !response.status().is_success() {
        return Err(BodyFailure::Status(response.status().as_u16()));
    }
    if let Some(length) = response
        .content_length()
        .filter(|length| *length > limit as u64)
    {
        return Err(BodyFailure::TooLarge { observed: length });
    }
    let mut bytes = asset_buffer(response.content_length(), limit);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| BodyFailure::Transport(request_failed(error)))?
    {
        let observed = bytes.len().saturating_add(chunk.len());
        if observed > limit {
            return Err(BodyFailure::TooLarge {
                observed: observed as u64,
            });
        }
        reserve_for_chunk(&mut bytes, chunk.len(), limit);
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, TransportError> {
    bounded_body(response, limit)
        .await
        .map_err(BodyFailure::into_service)
}

async fn json_response(response: reqwest::Response) -> Result<Value, TransportError> {
    let bytes = read_body(response, MAX_GRAPH_RESPONSE_BYTES).await?;
    serde_json::from_slice(&bytes).map_err(|source| {
        tracing::warn!(
            event = "gateway_whatsapp_media_refused",
            reason = "json",
            line = source.line(),
            column = source.column(),
            "gateway.whatsapp.media.refused"
        );
        TransportError::MalformedResponse(source)
    })
}

impl WhatsappDriver {
    fn download_url(&self, value: &str) -> Result<reqwest::Url, TransportError> {
        let url = reqwest::Url::parse(value).map_err(|source| {
            tracing::warn!(event = "gateway_whatsapp_media_refused", reason = "url-parse", cause = %source, "gateway.whatsapp.media.refused");
            TransportError::Response
        })?;
        let trusted = url.scheme() == "https"
            && url.host_str() == Some(DOWNLOAD_HOST)
            && url.port_or_known_default() == Some(443);
        // Only the test build may widen the trusted origin; production configuration can never add
        // another host to it.
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
            category = error.category(), delivered = accepted, "gateway.whatsapp.reply.partial");
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
                .mime_str(&image.media_type)
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
            let Some(kind) = Media::of(&mime).filter(|_| media_id(&id)) else {
                return Err(failure("asset-metadata"));
            };
            let limit = usize::try_from(kind.limit(max_bytes)).unwrap_or(usize::MAX);
            let response = self
                .http
                .get(format!("{}/{}/{}", self.endpoint, self.version, id))
                .query(&[("phone_number_id", &self.phone_number_id)])
                .bearer_auth(self.access_token.expose())
                .send()
                .await
                .map_err(request_failed)?;
            if !response.status().is_success() {
                let status = response.status().as_u16();
                return Err(kind.body_failure(BodyFailure::Status(status)));
            }
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
                return Err(kind.oversized(size));
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
            let bytes = bounded_body(response, limit)
                .await
                .map_err(|failure| kind.body_failure(failure))?;
            if bytes.len() as u64 != size {
                return Err(failure("media-size-mismatch"));
            }
            if crate::asset::signature_mime(&bytes) != Some(mime.as_str()) {
                return Err(kind.signature_failure());
            }
            Ok(bytes)
        })
    }
}

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
