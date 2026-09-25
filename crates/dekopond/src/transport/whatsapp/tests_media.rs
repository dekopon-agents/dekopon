use super::*;
use axum::body::Body;
use dekopon_agent::attachment::GeneratedImage;
use tokio::sync::oneshot;

pub(crate) const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake-png-pixels";
pub(crate) const JPEG: &[u8] = b"\xff\xd8\xfffake-jpeg-pixels";

pub(crate) struct MediaRequest {
    pub path: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

pub(crate) struct MediaPeer {
    pub origin: String,
    pub requests: Arc<Mutex<Vec<MediaRequest>>>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl MediaPeer {
    pub async fn new(reply: impl Fn(&str, usize) -> Response + Send + Sync + 'static) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("media mock binds");
        let origin = format!("http://{}", listener.local_addr().expect("address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let records = Arc::clone(&requests);
        let base = origin.clone();
        let reply = Arc::new(reply);
        let app = Router::new().fallback(move |request: Request| {
            let (records, base, reply) = (Arc::clone(&records), base.clone(), Arc::clone(&reply));
            async move {
                let (parts, body) = request.into_parts();
                let bytes = to_bytes(body, 6_000_000)
                    .await
                    .expect("bounded fixture request");
                let mut records = records.lock().expect("requests");
                let index = records.len();
                records.push(MediaRequest {
                    path: parts.uri.to_string(),
                    headers: parts.headers,
                    body: bytes.to_vec(),
                });
                reply(&base, index)
            }
        });
        let (stop, signal) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(dekopon_test_support::shutdown_on(signal))
                .await
                .expect("media mock serves");
        });
        Self {
            origin,
            requests,
            stop: Some(stop),
            task: Some(task),
        }
    }
    pub async fn finish(mut self) {
        self.stop
            .take()
            .expect("stop sender")
            .send(())
            .expect("mock running");
        self.task.take().expect("task").await.expect("mock joins");
    }
}
impl Drop for MediaPeer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(crate) fn json_reply(value: Value) -> Response {
    (StatusCode::OK, value.to_string()).into_response()
}
pub(crate) fn bytes_reply(bytes: &[u8]) -> Response {
    Response::new(Body::from_stream(futures_util::stream::iter(vec![Ok::<
        _,
        io::Error,
    >(
        bytes.to_vec(),
    )])))
}
pub(crate) fn metadata(origin: &str, mime: &str, bytes: &[u8]) -> Response {
    json_reply(
        json!({"id":"789", "mime_type":mime, "file_size":bytes.len().to_string(),
        "url":format!("{origin}/whatsapp_business/attachments/?mid=789&signed=URL_SENTINEL")}),
    )
}
pub(crate) fn accepted() -> Response {
    json_reply(json!({"messaging_product":"whatsapp", "messages":[{"id":"wamid.accepted"}]}))
}
fn driver(origin: &str) -> WhatsappDriver {
    WhatsappDriver {
        transport: "wa".to_owned(),
        endpoint: origin.to_owned(),
        version: "v25.0".to_owned(),
        phone_number_id: "456".to_owned(),
        access_token: Redacted::new("TOKEN_SENTINEL".to_owned()),
        http: credential_client(Duration::from_secs(1))
            .build()
            .expect("client"),
    }
}
fn source(mime: &str) -> AssetSourceRef {
    AssetSourceRef::WhatsApp {
        media_id: "789".to_owned(),
        mime: mime.to_owned(),
    }
}
fn png() -> GeneratedImage {
    GeneratedImage::from_png(PNG.to_vec()).expect("fixture PNG signature")
}
fn target() -> ReplyTarget {
    ReplyTarget::WhatsApp {
        recipient: "15550000001".to_owned(),
    }
}

pub(crate) fn photo_webhook(mime: &str, caption: Option<&str>) -> Value {
    let mut image = json!({"id":"789", "mime_type":mime, "url":"https://evil.invalid/ignored"});
    if let Some(caption) = caption {
        image["caption"] = json!(caption);
    }
    json!({"object":"whatsapp_business_account", "entry":[{"id":"123", "changes":[{"field":"messages", "value":{
        "messaging_product":"whatsapp", "metadata":{"phone_number_id":"456"}, "contacts":[{"wa_id":"15550000001"}],
        "messages":[{"from":"15550000001", "id":"wamid.photo", "type":"image", "image":image}]
    }}]}]})
}

#[tokio::test]
async fn signed_png_and_jpeg_captions_are_lazy_and_optional() {
    for mime in ["image/png", "image/jpeg"] {
        for caption in [None, Some(""), Some("Edit the sky 🟣")] {
            let (state, mut receiver) = super::tests::state_and_receiver();
            let body = photo_webhook(mime, caption).to_string();
            let response =
                receive_webhook(State(state), super::tests::signed_request(body.as_bytes())).await;
            assert_eq!(response.status(), StatusCode::OK);
            let mut delivery = receiver
                .try_recv()
                .expect("photo enqueued without fetching");
            let message = delivery.messages.pop_front().expect("message");
            assert_eq!(message.text, caption.unwrap_or_default());
            assert_eq!(message.assets.len(), 1);
            assert_eq!(message.assets[0].mime, mime);
            assert_eq!(message.assets[0].size, None);
            assert_eq!(message.assets[0].source, Some(source(mime)));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn six_staggered_signed_photos_form_one_lazy_batch_after_the_quiet_interval() {
    use crate::collection::{Collector, Offered};
    let config = serde_json::from_value(json!({
        "name":"wa", "kind":"whatsappCloudApi", "appSecretEnv":"APP",
        "verifyTokenEnv":"VERIFY", "accessTokenEnv":"ACCESS", "bind":"127.0.0.1:9080",
        "callbackPath":"/wa", "wabaId":"123", "phoneNumberId":"456", "graphApiVersion":"v25.0"
    }))
    .unwrap();
    let mut collector = Collector::new(&[config], 4);
    let (state, mut receiver) = super::tests::state_and_receiver();
    let start = tokio::time::Instant::now();
    let mut previous = 0;
    for (index, millis) in [0, 50, 3100, 3230, 3670, 4320].into_iter().enumerate() {
        tokio::time::advance(Duration::from_millis(millis - previous)).await;
        previous = millis;
        assert!(collector.take_due(tokio::time::Instant::now()).is_empty());
        let mut body = photo_webhook("image/jpeg", (index == 0).then_some("edit all six"));
        body["entry"][0]["changes"][0]["value"]["messages"][0]["id"] =
            json!(format!("wamid.photo{index}"));
        let response = receive_webhook(
            State(state.clone()),
            super::tests::signed_request(body.to_string().as_bytes()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut delivery = receiver.try_recv().unwrap();
        let message = delivery.messages.pop_front().unwrap();
        assert_eq!(message.assets[0].size, None);
        assert!(matches!(collector.offer(0, message), Offered::Pending));
    }
    let deadline = start + Duration::from_millis(9320);
    assert_eq!(collector.deadline(), Some(deadline));
    assert!(
        collector
            .take_due(deadline - Duration::from_nanos(1))
            .is_empty()
    );
    let ready = collector.take_due(deadline);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].assets.len(), 6);
    assert_eq!(ready[0].constituents.len(), 6);
    assert_eq!(ready[0].message_id.to_string(), "wamid.photo0");
    assert!(ready[0].text.contains("edit all six"));
    assert!(
        collector
            .take_due(deadline + Duration::from_secs(60))
            .is_empty()
    );
}

#[tokio::test]
async fn png_and_jpeg_downloads_validate_metadata_signature_and_phone_scope() {
    for (mime, bytes) in [("image/png", PNG), ("image/jpeg", JPEG)] {
        let peer = MediaPeer::new(move |origin, index| match index {
            0 => metadata(origin, mime, bytes),
            1 => bytes_reply(bytes),
            _ => panic!("unexpected request"),
        })
        .await;
        let driver = driver(&peer.origin);
        assert_eq!(
            driver
                .fetch(&source(mime), 8_000_000)
                .await
                .expect("download"),
            bytes
        );
        {
            let requests = peer.requests.lock().expect("requests");
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].path, "/v25.0/789?phone_number_id=456");
            for request in requests.iter() {
                assert_eq!(request.headers["authorization"], "Bearer TOKEN_SENTINEL");
            }
        }
        peer.finish().await;
    }
}

#[tokio::test]
async fn hostile_metadata_urls_never_receive_a_token_or_second_request() {
    for url in [
        "https://lookaside.fbsbx.com.evil.test/whatsapp_business/attachments/",
        "http://lookaside.fbsbx.com/whatsapp_business/attachments/",
        "https://lookaside.fbsbx.com@evil.test/whatsapp_business/attachments/",
        "https://user@lookaside.fbsbx.com/whatsapp_business/attachments/",
        "https://lookaside.fbsbx.com:444/whatsapp_business/attachments/",
        "https://127.0.0.1/whatsapp_business/attachments/",
        "https://lookaside.fbsbx.com/wrong/",
        "https://lookaside.fbsbx.com/whatsapp_business/attachments/#token",
    ] {
        let peer = MediaPeer::new(move |_, index| {
            assert_eq!(index, 0);
            json_reply(
                json!({"id":"789", "mime_type":"image/png", "file_size":PNG.len(), "url":url}),
            )
        })
        .await;
        let error = driver(&peer.origin)
            .fetch(&source("image/png"), 8_000_000)
            .await
            .expect_err("host trap refused");
        assert!(error.to_string().contains("media-destination"), "{error:?}");
        assert!(!format!("{error:?}").contains(url));
        assert_eq!(peer.requests.lock().expect("requests").len(), 1);
        peer.finish().await;
    }
}

#[tokio::test]
async fn redirects_at_metadata_and_download_are_not_followed() {
    for redirect_at in [0, 1] {
        let peer = MediaPeer::new(move |origin, index| {
            if index == redirect_at {
                (
                    StatusCode::FOUND,
                    [(header::LOCATION, format!("{origin}/trap"))],
                    "raw secret service body",
                )
                    .into_response()
            } else {
                assert_eq!(index, 0);
                metadata(origin, "image/png", PNG)
            }
        })
        .await;
        let error = driver(&peer.origin)
            .fetch(&source("image/png"), 8_000_000)
            .await
            .expect_err("redirect refused");
        assert!(error.to_string().contains("http-302"));
        assert!(!format!("{error:?}").contains("secret"));
        assert_eq!(
            peer.requests.lock().expect("requests").len(),
            redirect_at + 1
        );
        peer.finish().await;
    }
}

#[tokio::test]
async fn media_errors_are_bounded_and_name_the_failed_check() {
    for (case, reason) in [
        ("status", "http-401"),
        ("metadata-large", "response-too-large"),
        ("length", "image-too-large"),
        ("mime", "asset-metadata"),
        ("id", "asset-metadata"),
        ("signature", "image-signature"),
        ("stream", "response-too-large"),
        ("size", "media-size-mismatch"),
        ("json", "valid JSON"),
    ] {
        let peer = MediaPeer::new(move |origin,index| {
            if index == 0 {
                match case {
                    "status" => return (StatusCode::UNAUTHORIZED,"TOKEN_SENTINEL URL_SENTINEL raw response").into_response(),
                    "metadata-large" => return bytes_reply(&vec![b' '; MAX_GRAPH_RESPONSE_BYTES+1]),
                    "json" => return bytes_reply(b"<html>"),
                    _ => {}
                }
                let mut value = json!({"id":"789", "mime_type":"image/png", "file_size":PNG.len(), "url":format!("{origin}/whatsapp_business/attachments/?signed=URL_SENTINEL")});
                match case { "length" => value["file_size"]=json!(5_000_001), "mime" => value["mime_type"]=json!("image/jpeg"), "id" => value["id"]=json!("999"), _ => {} }
                json_reply(value)
            } else {
                assert_eq!(index,1);
                match case { "stream" => bytes_reply(&vec![0;5_000_001]), "signature" => bytes_reply(&vec![0;PNG.len()]), "size" => bytes_reply(b"short"), _=>panic!("unexpected download") }
            }
        }).await;
        let error = driver(&peer.origin)
            .fetch(&source("image/png"), 8_000_000)
            .await
            .expect_err(case);
        let debug = format!("{error:?}");
        assert!(error.to_string().contains(reason), "{case}: {error}");
        assert!(
            !debug.contains("TOKEN_SENTINEL") && !debug.contains("URL_SENTINEL"),
            "{debug}"
        );
        peer.finish().await;
    }
}

#[tokio::test]
async fn image_upload_is_multipart_and_only_message_acceptance_is_delivery() {
    for (caption, label, bytes, filename) in [
        (String::new(), "image/png", PNG, "asset-1.png"),
        ("🟣".repeat(1024), "image/png", PNG, "asset-1.png"),
        ("🟣".repeat(1025), "image/png", PNG, "asset-1.png"),
        (String::new(), "image/jpeg", JPEG, "asset-1.jpg"),
    ] {
        let peer = MediaPeer::new(|_, index| {
            if index == 0 {
                json_reply(json!({"id":"987"}))
            } else {
                accepted()
            }
        })
        .await;
        driver(&peer.origin)
            .reply(
                &target(),
                OutboundReply::with_images(
                    &caption,
                    vec![GeneratedImage::new(
                        dekopon_model::asset::DiskBlob::from_bytes(bytes).unwrap(),
                        label.to_owned(),
                        dekopon_broker_protocol::AssetEncoding::Identity,
                    )],
                ),
            )
            .await
            .expect("image accepted");
        {
            let requests = peer.requests.lock().expect("requests");
            assert_eq!(requests[0].path, "/v25.0/456/media");
            assert!(
                requests[0].headers["content-type"]
                    .to_str()
                    .expect("type")
                    .starts_with("multipart/form-data; boundary=")
            );
            let upload = String::from_utf8_lossy(&requests[0].body);
            assert!(upload.contains("name=\"messaging_product\"\r\n\r\nwhatsapp"));
            assert!(
                upload.contains(&format!("filename=\"{filename}\""))
                    && upload.contains(&format!("Content-Type: {label}"))
            );
            assert!(
                requests[0]
                    .body
                    .windows(bytes.len())
                    .any(|part| part == bytes)
            );
            let sent: Value = serde_json::from_slice(&requests[1].body).expect("message");
            assert_eq!(sent["to"], "15550000001");
            assert_eq!(sent["image"]["id"], "987");
            assert!(sent["image"].get("link").is_none());
            if caption.chars().count() == 1024 {
                assert_eq!(sent["image"]["caption"], caption);
            } else {
                assert!(sent["image"].get("caption").is_none());
            }
            if caption.chars().count() > 1024 {
                let sent: Value = serde_json::from_slice(&requests[2].body).expect("text");
                assert_eq!(sent["text"]["body"], caption);
            }
        }
        peer.finish().await;
    }
}

#[tokio::test]
async fn upload_success_is_not_delivery_and_later_failures_are_partial() {
    for fail_at in [0, 1, 2, 3, 4] {
        let peer = MediaPeer::new(move |_, index| {
            if index == fail_at {
                return (StatusCode::BAD_REQUEST, "secret service error").into_response();
            }
            if index % 2 == 0 {
                json_reply(json!({"id":"987"}))
            } else {
                accepted()
            }
        })
        .await;
        let text = "x".repeat(1025);
        let error = driver(&peer.origin)
            .reply(
                &target(),
                OutboundReply::with_images(text, vec![png(), png()]),
            )
            .await
            .expect_err("failure");
        assert_eq!(
            matches!(error, TransportError::PartialDelivery),
            fail_at >= 2,
            "failure at {fail_at}: {error}"
        );
        assert_eq!(
            peer.requests.lock().expect("requests").len(),
            fail_at + 1,
            "no retry"
        );
        peer.finish().await;
    }
}

fn encoded_image(bytes: &[u8], encoding: dekopon_broker_protocol::AssetEncoding) -> GeneratedImage {
    use dekopon_broker_protocol::AssetEncoding;
    use dekopon_core::base64::{Engine as _, STANDARD};
    let stored = match encoding {
        AssetEncoding::Identity => bytes.to_vec(),
        AssetEncoding::Base64 => STANDARD.encode(bytes).into_bytes(),
    };
    GeneratedImage::new(
        dekopon_model::asset::DiskBlob::from_bytes(&stored).unwrap(),
        "image/png".to_owned(),
        encoding,
    )
}

#[tokio::test]
async fn identity_and_base64_outputs_at_the_decoded_ceiling_upload_identical_bytes() {
    use dekopon_broker_protocol::AssetEncoding;
    let mut bytes = PNG.to_vec();
    bytes.resize(media::MAX_IMAGE_BYTES, 0);
    for encoding in [AssetEncoding::Identity, AssetEncoding::Base64] {
        let peer = MediaPeer::new(|_, index| match index {
            0 => json_reply(json!({"id":"987"})),
            1 => accepted(),
            _ => panic!("one upload and one captioned image, never a retry"),
        })
        .await;
        driver(&peer.origin)
            .reply(
                &target(),
                OutboundReply::with_images("answer", vec![encoded_image(&bytes, encoding)]),
            )
            .await
            .unwrap();
        {
            let requests = peer.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].path, "/v25.0/456/media");
            let body = &requests[0].body;
            let start = body
                .windows(PNG.len())
                .position(|part| part == PNG)
                .unwrap();
            assert_eq!(&body[start..start + bytes.len()], bytes);
            assert!(
                body[start + bytes.len()..].starts_with(b"\r\n--"),
                "no extra payload bytes"
            );
            let message: Value = serde_json::from_slice(&requests[1].body).unwrap();
            assert_eq!(message["image"]["caption"], "answer");
        }
        peer.finish().await;
    }
}

#[tokio::test]
async fn identity_and_base64_outputs_one_decoded_byte_over_refuse_before_any_upload_or_text() {
    use dekopon_broker_protocol::AssetEncoding;
    let mut bytes = PNG.to_vec();
    bytes.resize(media::MAX_IMAGE_BYTES + 1, 0);
    for encoding in [AssetEncoding::Identity, AssetEncoding::Base64] {
        let peer = MediaPeer::new(|_, _| panic!("preflight must send nothing")).await;
        let error = driver(&peer.origin)
            .reply(
                &target(),
                OutboundReply::with_images(
                    "x".repeat(media::MAX_CAPTION_CHARS + 1),
                    vec![png(), encoded_image(&bytes, encoding)],
                ),
            )
            .await
            .expect_err("too large");
        assert!(
            matches!(error, TransportError::Service { ref code } if code == "image-too-large"),
            "{error:?}"
        );
        assert!(peer.requests.lock().unwrap().is_empty());
        peer.finish().await;
    }
}

pub(crate) async fn admitted_photo(
    origin: &str,
    mime: &str,
    caption: Option<&str>,
) -> (WhatsappTransport, InboundMessage) {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve address");
    let address = reservation.local_addr().expect("address");
    drop(reservation);
    let mut transport = WhatsappTransport::new(
        "wa".to_owned(),
        address,
        "/wa".to_owned(),
        "123".to_owned(),
        "456".to_owned(),
        "v25.0".to_owned(),
        origin.to_owned(),
        "secret".to_owned(),
        "verify".to_owned(),
        "TOKEN_SENTINEL".to_owned(),
        LivenessSettings::default(),
    )
    .expect("transport");
    transport.connect().await.expect("listener");
    let body = photo_webhook(mime, caption).to_string();
    let digest = hmac_sha256(b"secret", body.as_bytes());
    let signature: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let response = credential_client(Duration::from_secs(2))
        .build()
        .expect("webhook client")
        .post(format!("http://{address}/wa"))
        .header("x-hub-signature-256", format!("sha256={signature}"))
        .body(body)
        .send()
        .await
        .expect("webhook");
    assert_eq!(response.status(), StatusCode::OK);
    let TransportEvent::Message(message) = transport.next().await.expect("photo") else {
        panic!("photo message")
    };
    (transport, *message)
}

#[tokio::test]
async fn invalid_image_envelopes_and_unsigned_photos_enqueue_nothing() {
    for case in [
        "unsigned", "scope", "phone", "mime", "media-id", "caption", "group", "contact", "self",
    ] {
        let (state, mut receiver) = super::tests::state_and_receiver();
        let mut body = photo_webhook("image/png", Some("edit"));
        let value = &mut body["entry"][0]["changes"][0]["value"];
        match case {
            "phone" => value["metadata"]["phone_number_id"] = json!("999"),
            "mime" => value["messages"][0]["image"]["mime_type"] = json!("image/webp"),
            "media-id" => value["messages"][0]["image"]["id"] = json!("../trap?token"),
            "caption" => value["messages"][0]["image"]["caption"] = json!(32),
            "group" => value["messages"][0]["group_id"] = json!("group"),
            "contact" => value["contacts"] = json!([]),
            "self" => value["metadata"]["display_phone_number"] = json!("+1 555 000 0001"),
            _ => {}
        }
        if case == "scope" {
            body["entry"][0]["id"] = json!("999");
        }
        let body = body.to_string();
        let mut request = super::tests::signed_request(body.as_bytes());
        if case == "unsigned" {
            request.headers_mut().remove("x-hub-signature-256");
        }
        let response = receive_webhook(State(state), request).await;
        assert_eq!(
            response.status(),
            if case == "unsigned" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::OK
            }
        );
        assert!(receiver.try_recv().is_err(), "{case}");
    }
}

#[tokio::test]
async fn malformed_upload_or_message_success_is_not_delivery() {
    for (case, reason) in [
        ("upload-id", "upload-id"),
        ("upload-large", "response-too-large"),
        ("message-id", "message-acceptance"),
    ] {
        let peer = MediaPeer::new(move |_, index| match (case, index) {
            ("upload-id", 0) => json_reply(json!({"id":"../invalid"})),
            ("upload-large", 0) => bytes_reply(&vec![b' '; MAX_GRAPH_RESPONSE_BYTES + 1]),
            ("message-id", 0) => json_reply(json!({"id":"987"})),
            ("message-id", 1) => json_reply(json!({"messaging_product":"whatsapp","messages":[]})),
            _ => panic!("must not retry malformed acceptance"),
        })
        .await;
        let error = driver(&peer.origin)
            .reply(&target(), OutboundReply::with_images("", vec![png()]))
            .await
            .expect_err("not delivery");
        assert!(error.to_string().contains(reason), "{error}");
        assert!(!matches!(error, TransportError::PartialDelivery));
        peer.finish().await;
    }
}

#[tokio::test]
async fn a_stalled_download_times_out_without_exposing_the_signed_url() {
    let peer = MediaPeer::new(|origin, index| match index {
        0 => metadata(origin, "image/png", PNG),
        1 => Response::new(Body::from_stream(futures_util::stream::pending::<
            Result<Vec<u8>, io::Error>,
        >())),
        _ => panic!("no automatic retry"),
    })
    .await;
    let mut driver = driver(&peer.origin);
    driver.http = credential_client(Duration::from_millis(30))
        .build()
        .expect("short deadline client");
    let error = driver
        .fetch(&source("image/png"), 8_000_000)
        .await
        .expect_err("download timeout");
    let debug = format!("{error:?}");
    assert!(matches!(error, TransportError::Request(_)), "{debug}");
    assert!(
        debug.contains("TimedOut") || debug.contains("timeout"),
        "{debug}"
    );
    assert!(
        !debug.contains("URL_SENTINEL") && !debug.contains("TOKEN_SENTINEL"),
        "{debug}"
    );
    assert_eq!(peer.requests.lock().expect("requests").len(), 2);
    drop(peer);
}
