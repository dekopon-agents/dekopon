use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;

use super::*;
use crate::transport::{ChatTransport as _, webhook::hmac_sha256};

fn whatsapp() -> (WhatsappTransport, std::net::SocketAddr) {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
    let address = probe.local_addr().expect("address");
    drop(probe);
    let transport = WhatsappTransport::new(
        "wa".to_owned(),
        address,
        "/wa".to_owned(),
        "123".to_owned(),
        "456".to_owned(),
        "v23.0".to_owned(),
        "http://127.0.0.1:9".to_owned(),
        "secret".to_owned(),
        "verify".to_owned(),
        "access".to_owned(),
        LivenessSettings {
            mode: LivenessMode::Native,
            ..LivenessSettings::default()
        },
    )
    .expect("transport");
    (transport, address)
}

fn whatsapp_delivery() -> (Vec<u8>, String) {
    let body = serde_json::to_vec(&json!({
        "object":"whatsapp_business_account",
        "entry":[{"id":"123","changes":[{"field":"messages","value":{
            "messaging_product":"whatsapp","metadata":{"phone_number_id":"456"},
            "contacts":[{"wa_id":"1603"}],
            "messages":[{"id":"wamid.captured","from":"1603","timestamp":"1700000000",
                "type":"text","text":{"body":"pay the invoice"}}]
        }}]}]
    }))
    .expect("body");
    let hex: String = hmac_sha256(b"secret", &body)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    (body, format!("sha256={hex}"))
}

#[tokio::test]
async fn identical_signed_whatsapp_message_is_queued_once() {
    let (mut transport, address) = whatsapp();
    transport.connect().await.expect("connect");
    let (body, signature) = whatsapp_delivery();
    let client = reqwest::Client::new();
    let mut queued = 0_usize;
    for _ in 0..2 {
        let response = client
            .post(format!("http://{address}/wa"))
            .header("x-hub-signature-256", &signature)
            .body(body.clone())
            .send()
            .await
            .expect("POST");
        assert_eq!(response.status(), StatusCode::OK);
        if let Ok(Ok(TransportEvent::Message(message))) =
            tokio::time::timeout(Duration::from_millis(500), transport.next()).await
        {
            assert_eq!(message.message_id.to_string(), "wamid.captured");
            queued += 1;
        }
    }
    assert_eq!(
        queued, 1,
        "the same signed message id was handed to the gateway twice"
    );
}
