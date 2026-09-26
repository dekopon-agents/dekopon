#![allow(clippy::unwrap_used)]

use std::{sync::OnceLock, time::Duration};

use dekopon_capability::HttpConstraints;
use dekopon_http_host::{
    BufferedHttpClient, ErrorCode, Header, HttpHostCeilings, Request, StreamedRequest,
    asset::AssetDirectory,
};
use dekopon_test_support::LoopbackServer;
use opentelemetry::trace::{
    SpanContext, SpanId, TraceContextExt as _, TraceId, TracerProvider as _,
};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::{Instrument as _, instrument::WithSubscriber as _};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

fn install() {
    static INSTALLED: OnceLock<SdkTracerProvider> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let provider = SdkTracerProvider::builder().build();
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("http-trace-test")))
            .init();
        provider
    });
}

fn grant(authority: &str) -> HttpConstraints {
    HttpConstraints {
        allowed_hosts: vec![authority.to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        max_requests: 2,
        max_request_bytes: 1024,
        max_response_bytes: 1024,
        allow_plaintext_loopback: true,
        propagate_trace: false,
    }
}

fn client(grant: HttpConstraints) -> BufferedHttpClient {
    BufferedHttpClient::authorized(grant, HttpHostCeilings::default(), Duration::from_secs(5))
        .unwrap()
}

fn request(uri: String) -> Request {
    Request {
        method: "GET".to_owned(),
        uri,
        headers: Vec::new(),
        body: Vec::new(),
    }
}

fn header<'a>(wire: &'a str, name: &str) -> Option<&'a str> {
    wire.split("\r\n")
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

fn assert_egress_parent(wire: &str, enclosing: &SpanContext) {
    let value = header(wire, "traceparent").expect("broker traceparent");
    let parts = value.split('-').collect::<Vec<_>>();
    assert_eq!(parts.len(), 4);
    assert_eq!(parts[0], "00");
    assert_eq!(parts[1].len(), 32);
    assert_eq!(parts[2].len(), 16);
    assert_eq!(parts[3].len(), 2);
    assert_eq!(value, value.to_ascii_lowercase());
    assert_eq!(TraceId::from_hex(parts[1]).unwrap(), enclosing.trace_id());
    let parent = SpanId::from_hex(parts[2]).unwrap();
    assert_ne!(parent, SpanId::INVALID);
    assert_ne!(parent, enclosing.span_id());
    assert_eq!(
        u8::from_str_radix(parts[3], 16).unwrap(),
        enclosing.trace_flags().to_u8()
    );
    assert!(header(wire, "tracestate").is_none());
}

#[tokio::test]
async fn only_opted_in_requests_send_the_egress_parent_outside_accounted_bytes() {
    install();
    let enclosing = tracing::info_span!("invocation");
    let context = enclosing.context().span().span_context().clone();
    assert!(context.is_valid());
    let server = LoopbackServer::serving(
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        3,
    );
    let mut plain = client(grant(server.authority()));
    plain
        .send(request(server.url()))
        .instrument(enclosing.clone())
        .await
        .unwrap();
    let wire = String::from_utf8(server.request()).unwrap();
    assert!(header(&wire, "traceparent").is_none());
    assert!(header(&wire, "tracestate").is_none());
    let accounted = plain.into_evidence()[0].request_bytes;

    let mut opted_in = grant(server.authority());
    opted_in.propagate_trace = true;
    opted_in.max_request_bytes = accounted;
    let mut opted_in = client(opted_in);
    for _ in 0..2 {
        opted_in
            .send(request(server.url()))
            .instrument(enclosing.clone())
            .await
            .unwrap();
        assert_egress_parent(&String::from_utf8(server.request()).unwrap(), &context);
    }
    for evidence in opted_in.into_evidence() {
        assert_eq!(evidence.request_bytes, accounted);
    }
    server.join();
}

#[tokio::test]
async fn an_opted_in_request_without_an_otel_layer_proceeds_without_trace_headers() {
    install();
    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    );
    let mut grant = grant(server.authority());
    grant.propagate_trace = true;
    let response = client(grant)
        .send(request(server.url()))
        .with_subscriber(tracing_subscriber::registry())
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    let wire = String::from_utf8(server.request()).unwrap();
    assert!(header(&wire, "traceparent").is_none());
    assert!(header(&wire, "tracestate").is_none());
    server.join();
}

#[tokio::test]
async fn guest_trace_headers_are_refused_under_every_grant() {
    install();
    for propagate_trace in [false, true] {
        for name in ["traceparent", "tracestate", "TraceParent", "TraceState"] {
            let mut grant = grant("127.0.0.1:9");
            grant.propagate_trace = propagate_trace;
            let mut request = request("http://127.0.0.1:9/".to_owned());
            request.headers.push(Header {
                name: name.to_owned(),
                value: b"guest-forged".to_vec(),
            });
            let error = client(grant).send(request).await.unwrap_err();
            assert!(matches!(error.code, ErrorCode::InvalidHeader));
        }
    }
}

#[tokio::test]
async fn opted_in_streaming_requests_send_the_egress_parent() {
    install();
    let enclosing = tracing::info_span!("streaming_invocation");
    let context = enclosing.context().span().span_context().clone();
    let server = LoopbackServer::once(
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    );
    let mut grant = grant(server.authority());
    grant.propagate_trace = true;
    let root = tempfile::tempdir().unwrap();
    let directory = AssetDirectory::new(root.path().to_owned(), 2);
    let response = client(grant)
        .stream(
            StreamedRequest {
                method: "GET".to_owned(),
                uri: server.url(),
                headers: Vec::new(),
                body: Vec::new(),
            },
            &directory,
        )
        .instrument(enclosing)
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_egress_parent(&String::from_utf8(server.request()).unwrap(), &context);
    server.join();
}
