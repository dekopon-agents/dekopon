use std::time::Duration;

use dekopon_core::{CapabilityId, InvocationId, TraceId};
use serde_json::json;
use tokio::io::{AsyncWriteExt as _, duplex};

use super::{
    AgentInventory, BrokerRequest, FrameLimits, InvocationRequest, MAX_REPORTED_MODEL_CALLS,
    MAX_REPORTED_TEXT_BYTES, MAX_REPORTED_TOKENS, ModelUsageReport, Permission, ProtocolError,
    ReportedAgent, ReportedAgentCapability, RequestEnvelope, ResponseEnvelope, TraceParent,
    TraceParentError, read_frame, write_frame,
};

fn invocation() -> InvocationRequest {
    InvocationRequest {
        id: "invoke-test"
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability: "echo.echo"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        trace: "trace-test"
            .parse::<TraceId>()
            .expect("valid trace fixture"),
        trace_parent: None,
        input: json!({"message": "hello"}),
    }
}

const SAMPLE_TRACE_PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

#[test]
fn trace_parent_round_trips_through_its_wire_form() {
    let parsed = SAMPLE_TRACE_PARENT
        .parse::<TraceParent>()
        .expect("valid traceparent");

    assert_eq!(parsed.to_string(), SAMPLE_TRACE_PARENT);
    assert_eq!(parsed.flags(), 1);
    assert_eq!(parsed.trace_id()[0], 0x4b);
    assert_eq!(parsed.parent_id()[7], 0xb7);
}

/// Every rejection here is a value that would otherwise correlate broker spans to a trace that
/// does not exist, or serialize one logical context two different ways.
#[test]
fn trace_parent_rejects_malformed_unsupported_and_zero_values() {
    for invalid in [
        "",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
        "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
        "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e47zz-00f067aa0ba902b7-01",
        "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
    ] {
        assert!(
            invalid.parse::<TraceParent>().is_err(),
            "accepted {invalid:?}"
        );
    }

    assert_eq!(
        "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
            .parse::<TraceParent>()
            .expect_err("future version is rejected"),
        TraceParentError::UnsupportedVersion {
            version: "01".to_owned()
        }
    );
}

/// `traceParent` is always written and decodes from both forms.
///
/// Serde treats an `Option` field as implicitly optional, so an omitted key and an explicit `null`
/// both mean "this client exports no telemetry" — the same thing. That is the intended reading:
/// absence is a real state, not a client bug worth failing a decode over. What must hold is that
/// the field is never silently dropped when it *is* set.
#[test]
fn invocation_request_always_writes_trace_parent_and_decodes_both_forms() {
    let complete = serde_json::to_value(invocation()).expect("request serializes");
    assert_eq!(complete.get("traceParent"), Some(&json!(null)));

    let mut omitted = complete.clone();
    omitted
        .as_object_mut()
        .expect("request object")
        .remove("traceParent");
    assert!(
        serde_json::from_value::<InvocationRequest>(omitted)
            .expect("omitted traceparent decodes")
            .trace_parent
            .is_none()
    );

    let mut populated = complete;
    populated
        .as_object_mut()
        .expect("request object")
        .insert("traceParent".to_owned(), json!(SAMPLE_TRACE_PARENT));
    let decoded =
        serde_json::from_value::<InvocationRequest>(populated).expect("populated request decodes");
    let parent = decoded.trace_parent.expect("traceparent present");
    assert_eq!(parent.flags(), 1);
    assert_eq!(parent.to_string(), SAMPLE_TRACE_PARENT);

    // An invalid value is still a decode failure: a malformed parent would attach broker spans to
    // a trace that does not exist, which is worse than sending none.
    let mut malformed = serde_json::to_value(invocation()).expect("request serializes");
    malformed
        .as_object_mut()
        .expect("request object")
        .insert("traceParent".to_owned(), json!("not-a-traceparent"));
    assert!(serde_json::from_value::<InvocationRequest>(malformed).is_err());
}

#[tokio::test]
async fn round_trips_one_strict_bounded_frame() {
    let limits = FrameLimits {
        max_frame_bytes: 4 * 1024,
        io_timeout: Duration::from_secs(1),
    };
    let expected = RequestEnvelope::invoke(invocation());
    let (mut writer, mut reader) = duplex(8 * 1024);
    let write = tokio::spawn({
        let expected = expected.clone();
        async move { write_frame(&mut writer, &expected, limits).await }
    });
    let actual = read_frame::<_, RequestEnvelope>(&mut reader, limits)
        .await
        .expect("bounded request decodes");
    write
        .await
        .expect("writer task exits")
        .expect("frame writes");
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn rejects_oversized_prefix_before_reading_a_body() {
    let limits = FrameLimits {
        max_frame_bytes: 16,
        io_timeout: Duration::from_secs(1),
    };
    let (mut writer, mut reader) = duplex(32);
    writer
        .write_all(&100_u32.to_be_bytes())
        .await
        .expect("write oversized prefix");
    let error = read_frame::<_, RequestEnvelope>(&mut reader, limits)
        .await
        .expect_err("oversized prefix must fail before body allocation");
    assert!(matches!(
        error,
        ProtocolError::FrameTooLarge {
            length: 100,
            maximum: 16
        }
    ));
}

#[tokio::test]
async fn complete_frame_read_has_one_deadline() {
    let limits = FrameLimits {
        max_frame_bytes: 1024,
        io_timeout: Duration::from_millis(10),
    };
    let (_writer, mut reader) = duplex(32);
    let error = read_frame::<_, RequestEnvelope>(&mut reader, limits)
        .await
        .expect_err("idle peer must time out");
    assert!(matches!(error, ProtocolError::Timeout));
}

#[tokio::test]
async fn serialization_stops_at_the_frame_bound() {
    let limits = FrameLimits {
        max_frame_bytes: 32,
        io_timeout: Duration::from_secs(1),
    };
    let (mut writer, _reader) = duplex(64);
    let error = write_frame(&mut writer, &json!({"value": "x".repeat(256)}), limits)
        .await
        .expect_err("serializer must stop at bound");
    assert!(matches!(error, ProtocolError::FrameTooLarge { .. }));
}

#[test]
fn wire_invocation_contains_no_identity_or_authority_fields() {
    let value =
        serde_json::to_value(RequestEnvelope::invoke(invocation())).expect("request serializes");
    let encoded = serde_json::to_string(&value).expect("request JSON renders");
    for prohibited in [
        "principal",
        "actor",
        "authorizedInvocation",
        "constraints",
        "credential",
    ] {
        assert!(!encoded.contains(prohibited), "wire leaked {prohibited}");
    }
    assert!(
        serde_json::from_value::<RequestEnvelope>(json!({
            "apiVersion": "dekopon.dev/broker/v1alpha1",
            "request": {
                "operation": "invoke",
                "invocation": {
                    "id": "invoke-test",
                    "capability": "echo.echo",
                    "trace": "trace-test",
                    "input": {},
                    "actor": {"type": "service", "principal": "forged"}
                }
            }
        }))
        .is_err()
    );
}

#[test]
fn informational_reports_are_bounded_and_carry_no_authority_or_prompt_content() {
    let inventory = AgentInventory {
        agents: vec![ReportedAgent {
            id: "reviewer".parse().expect("valid agent"),
            description: "Reviews pull requests".to_owned(),
            enabled: true,
            model_class: Some("reasoning".to_owned()),
            providers: vec!["gh".parse().expect("valid provider")],
            capabilities: vec![ReportedAgentCapability {
                id: "gh.pull-request.read".parse().expect("valid capability"),
                provider: "gh".parse().expect("valid provider"),
                permissions: vec![Permission {
                    operation: "pull_requests:read".to_owned(),
                    resource: Some("dekopon-agents/*".to_owned()),
                }],
            }],
        }],
        truncated: false,
    };
    assert!(inventory.is_valid());
    let encoded =
        serde_json::to_string(&RequestEnvelope::publish_agent_inventory(inventory.clone()))
            .expect("inventory serializes");
    for prohibited in [
        "instructions",
        "prompt",
        "credential",
        "principal",
        "policy",
    ] {
        assert!(
            !encoded.contains(prohibited),
            "inventory leaked {prohibited}"
        );
    }

    let mut duplicated = inventory.clone();
    duplicated.agents.push(inventory.agents[0].clone());
    assert!(!duplicated.is_valid());
    let mut oversized = inventory;
    oversized.agents[0].description = "x".repeat(MAX_REPORTED_TEXT_BYTES + 1);
    assert!(!oversized.is_valid());

    let usage = ModelUsageReport {
        model_calls: 2,
        input_tokens: 100,
        input_unreported_calls: 1,
        output_tokens: 12,
        ..ModelUsageReport::default()
    };
    assert!(usage.is_valid());
    assert!(matches!(
        RequestEnvelope::publish_model_usage(usage).request,
        BrokerRequest::PublishModelUsage { .. }
    ));
    assert!(!ModelUsageReport::default().is_valid());
    assert!(
        !ModelUsageReport {
            model_calls: MAX_REPORTED_MODEL_CALLS + 1,
            ..ModelUsageReport::default()
        }
        .is_valid()
    );
    assert!(
        !ModelUsageReport {
            model_calls: 1,
            input_tokens: MAX_REPORTED_TOKENS + 1,
            ..ModelUsageReport::default()
        }
        .is_valid()
    );
    assert!(
        !ModelUsageReport {
            model_calls: 1,
            output_unreported_calls: 2,
            ..ModelUsageReport::default()
        }
        .is_valid()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unix_client_authenticates_private_socket_and_response_variant() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    use tokio::net::UnixListener;

    use super::BrokerClient;

    let directory = tempfile::tempdir().expect("create socket fixture directory");
    let socket = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).expect("bind broker fixture");
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .expect("make fixture socket private");
    let uid = std::fs::metadata(&socket).expect("socket metadata").uid();
    let limits = FrameLimits {
        max_frame_bytes: 4 * 1024,
        io_timeout: Duration::from_secs(1),
    };
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept client fixture");
        let request = read_frame::<_, RequestEnvelope>(&mut stream, limits)
            .await
            .expect("server decodes request");
        assert!(matches!(request.request, BrokerRequest::Capabilities));
        write_frame(
            &mut stream,
            &ResponseEnvelope::capabilities(Vec::new(), Vec::new()),
            limits,
        )
        .await
        .expect("server writes response");
    });

    let client = BrokerClient::new(&socket, uid, limits).expect("valid client limits");
    assert!(
        client
            .capabilities()
            .await
            .expect("authenticated exchange succeeds")
            .is_empty()
    );
    server.await.expect("server fixture exits");

    let wrong_uid = uid.wrapping_add(1);
    let client = BrokerClient::new(&socket, wrong_uid, limits).expect("valid client limits");
    assert!(client.capabilities().await.is_err());
}
