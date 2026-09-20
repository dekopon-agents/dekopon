// Match the workspace's test-only unwrap policy; production code keeps the deny lint.
#![allow(clippy::unwrap_used)]

use dekopon_broker_host::asset::AssetInputs;
use dekopon_broker_host::{BrokerHostError, BrokerHostLimits, BrokerProviderRegistry};
use dekopon_broker_protocol::{AssetEncoding, AssetRow};
use dekopon_capability::AssetConstraints;
use dekopon_capability::{
    AuthorizedInvocation, ExecutionConstraints, HttpConstraints, ProposedInvocation,
    broker::AuthorizationGate,
};
use dekopon_core::base64::{Engine as _, STANDARD};
use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};
use dekopon_http_host::asset::AssetDirectory;
use dekopon_test_support::{CaptureLayer, LoopbackServer, provider_fixture};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

fn authorized(
    capability: CapabilityId,
    input: Value,
    constraints: ExecutionConstraints,
) -> AuthorizedInvocation {
    let provider = capability
        .as_str()
        .split('.')
        .next()
        .expect("fixture capability has a provider prefix")
        .to_owned();
    authorized_for(&provider, capability, input, constraints)
}

fn authorized_for(
    provider: &str,
    capability: CapabilityId,
    input: Value,
    constraints: ExecutionConstraints,
) -> AuthorizedInvocation {
    let proposal = ProposedInvocation::new(
        "invoke-test"
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability,
        Actor::Agent {
            agent: "provider-test"
                .parse::<AgentId>()
                .expect("valid agent fixture"),
        },
        "0000000000000000000000000000f1c7"
            .parse::<TraceId>()
            .expect("valid trace fixture"),
        input,
    );
    AuthorizationGate::new()
        .authorize(
            proposal,
            provider.parse().expect("valid provider fixture"),
            "decision-test".to_owned(),
            "broker-test"
                .parse::<PrincipalId>()
                .expect("valid principal fixture"),
            "policy-test".to_owned(),
            constraints,
        )
        .expect("test broker authorizes bounded fixture")
}

fn http_constraints(authority: String, method: &str) -> ExecutionConstraints {
    ExecutionConstraints {
        asset: None,
        timeout_ms: 5_000,
        max_output_bytes: 1024 * 1024,
        http: Some(HttpConstraints {
            allowed_hosts: vec![authority],
            allowed_methods: vec![method.to_owned()],
            max_requests: 1,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
        }),
        storage: None,
        secret_use: None,
    }
}

// One test owns the process-wide capture; real component imports, never direct StoreState calls.
#[tokio::test(flavor = "multi_thread")]
async fn direct_wit_lists_are_bounded_before_payload_copy_and_non_http_reads_record_decoded_hashes()
{
    let capture = CaptureLayer::workspace();
    tracing_subscriber::registry().with(capture.clone()).init();
    let root = tempfile::tempdir().unwrap();
    let directory = AssetDirectory::new(root.path().to_owned(), 65536);
    let mut registry = BrokerProviderRegistry::load(
        [provider_fixture("http-probe-provider.wasm")],
        BrokerHostLimits {
            fuel: 1_000_000_000,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    registry.set_assets(directory.clone());
    let constraints = ExecutionConstraints {
        asset: Some(AssetConstraints {
            attach: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    for bytes in [65536, 65537, 8 * 1024 * 1024] {
        let result = registry
            .invoke(
                authorized(
                    "http-probe.fetch".parse().unwrap(),
                    json!({"assetMode":"direct-write", "bytes":bytes}),
                    constraints.clone(),
                ),
                None,
                AssetInputs::default(),
            )
            .await;
        if bytes == 65536 {
            assert_eq!(result.unwrap().assets.attached[0].bytes, bytes as u64);
        } else {
            assert!(matches!(
                *result.unwrap_err().error,
                BrokerHostError::HostCallRejected {
                    reason: "asset-call-rejected",
                    ..
                }
            ));
        }
        assert!(capture.events().iter().any(|(fields, _)| {
            fields.contains(&format!("asset.write.guest_bytes={bytes}"))
                && fields.contains(&format!(
                    "asset.write.copied_bytes={}",
                    if bytes == 65536 { bytes } else { 0 }
                ))
        }));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        directory
            .allocate()
            .await
            .unwrap()
            .write(vec![0; 65536])
            .await
            .unwrap();
    }
    let payload = b"non-http-private-asset-payload-not-for-traces";
    let hash = Sha256::digest(payload)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    for (id, encoding, bytes) in [
        (41, AssetEncoding::Identity, payload.to_vec()),
        (
            42,
            AssetEncoding::Base64,
            STANDARD.encode(payload).into_bytes(),
        ),
    ] {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &bytes).unwrap();
        let inputs = AssetInputs {
            rows: vec![AssetRow {
                id,
                content_type: "text/plain".to_owned(),
                encoding,
                bytes: bytes.len() as u64,
                origin: "chat".to_owned(),
                sent: false,
            }],
            descriptors: vec![std::fs::File::open(file.path()).unwrap().into()],
            sends_remaining: 0,
        };
        let result = registry
            .invoke(
                authorized(
                    "http-probe.fetch".parse().unwrap(),
                    json!({"assetMode":"read", "reference":format!("chat-asset:{id}")}),
                    constraints.clone(),
                ),
                None,
                inputs,
            )
            .await
            .unwrap();
        assert_eq!(result.output, json!({"read":payload.len()}));
        let records = capture.records();
        let hashes = records.iter().filter(|record| matches!(record, dekopon_test_support::Record::Event { fields, scope, .. } if fields.contains(&format!("asset.id={id}")) && fields.contains(&hash) && fields.contains(r#"asset.content_type="text/plain""#) && scope.contains(&"provider.invoke"))).count();
        assert_eq!(hashes, 1);
    }
    assert!(
        !capture
            .events_text()
            .contains(std::str::from_utf8(payload).unwrap())
    );
    assert!(
        !capture
            .spans_text()
            .contains(std::str::from_utf8(payload).unwrap())
    );
    assert!(!capture.events_text().contains(&STANDARD.encode(payload)));

    // Both caught native writer exhaustion and caught HTTP spool exhaustion are terminal.
    let tight_directory = AssetDirectory::new(root.path().to_owned(), 1);
    registry.set_assets(tight_directory.clone());
    for after in ["return", "spin", "http-denied"] {
        let mut bounded = constraints.clone();
        if after == "spin" {
            bounded.timeout_ms = 50;
        }
        let result = registry
            .invoke(
                authorized(
                    "http-probe.fetch".parse().unwrap(),
                    json!({"assetMode":"direct-write", "bytes":2, "afterWriteError":after}),
                    bounded,
                ),
                None,
                AssetInputs::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(*result.error, BrokerHostError::AssetOverBudget));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        tight_directory
            .allocate()
            .await
            .unwrap()
            .write(vec![0])
            .await
            .unwrap();
    }
    for caught in [false, true] {
        let server = LoopbackServer::once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let mut constraints = http_constraints(server.authority().to_owned(), "POST");
        constraints.asset = Some(AssetConstraints {
            attach: true,
            ..Default::default()
        });
        let result = registry.invoke(authorized("http-probe.fetch".parse().unwrap(), json!({"assetMode":"stream", "references":[], "uri":server.url(), "catch_stream_error":caught}), constraints), None, AssetInputs::default()).await.unwrap_err();
        assert!(matches!(*result.error, BrokerHostError::AssetOverBudget));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        server.join();
    }
}
