use std::time::Duration;

use dekopon_core::{CapabilityId, InvocationId, SecretUseProposal, TraceId};
use serde_json::json;
use tokio::io::{AsyncWriteExt as _, duplex};

use super::{
    AgentInventory, Attestation, BrokerRequest, ChatScopeClaim, ChatTransportKind,
    CommandRunOutcome, ComponentFailure, DeliveredTurnRequest, DeliveryIdentity, FrameLimits,
    InventoryError, InvocationRequest, MAX_REPORTED_AGENT_PROVIDERS, MAX_REPORTED_MODEL_CALLS,
    MAX_REPORTED_TEXT_BYTES, MAX_REPORTED_TOKENS, ModelUsageReport, PROTOCOL_VERSION, Permission,
    ProtocolError, ProtocolVersion, ReportedAgent, ReportedAgentCapability, RequestEnvelope,
    ResponseEnvelope, TraceParent, TraceParentError, UsageReportError, read_frame, write_frame,
};

fn subject() -> dekopon_core::ExternalSubject {
    "slack.t0123abc.u9xyz"
        .parse()
        .expect("valid subject fixture")
}

fn agent() -> dekopon_core::AgentId {
    "reviewer".parse().expect("valid agent fixture")
}

fn scope() -> ChatScopeClaim {
    ChatScopeClaim {
        transport: "scientist-slack".parse().expect("valid transport fixture"),
        kind: ChatTransportKind::Slack,
        channel: "c0123abc".to_owned(),
        conversation: "c0123abc:1712345678.000100".to_owned(),
    }
}

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
        secret_use: None,
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
    let expected = RequestEnvelope::invoke(None, invocation());
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

/// A prefix is a claim, not a measurement.
///
/// An in-bound length is accepted, so it decides nothing about allocation: the reader must follow
/// the bytes that actually arrive and refuse a frame that ends early rather than decoding a prefix
/// of it. This is what keeps 64 connected peers that each announce a 2 MiB frame and then send
/// nothing from pinning 128 MiB of zeroed buffers until the deadline.
#[tokio::test]
async fn in_bound_prefix_that_over_promises_fails_instead_of_decoding_a_short_frame() {
    let limits = FrameLimits {
        max_frame_bytes: 8 * 1024 * 1024,
        io_timeout: Duration::from_secs(1),
    };
    let (mut writer, mut reader) = duplex(1024);
    let payload = br#"{"apiVersion":"#;
    writer
        .write_all(&(8_u32 * 1024 * 1024).to_be_bytes())
        .await
        .expect("write in-bound prefix");
    writer
        .write_all(payload)
        .await
        .expect("write short payload");
    drop(writer);

    let error = read_frame::<_, RequestEnvelope>(&mut reader, limits)
        .await
        .expect_err("a frame shorter than its prefix must fail");
    assert!(
        matches!(&error, ProtocolError::Io { source } if source.kind() == std::io::ErrorKind::UnexpectedEof),
        "expected an unexpected-EOF failure, got {error}"
    );
}

/// A peer that sends only a prefix holds the connection, never a frame's worth of memory.
#[tokio::test]
async fn prefix_only_peer_times_out_rather_than_completing_a_frame() {
    let limits = FrameLimits {
        max_frame_bytes: 2 * 1024 * 1024,
        io_timeout: Duration::from_millis(50),
    };
    let (mut writer, mut reader) = duplex(1024);
    writer
        .write_all(&(2_u32 * 1024 * 1024).to_be_bytes())
        .await
        .expect("write in-bound prefix");

    let error = read_frame::<_, RequestEnvelope>(&mut reader, limits)
        .await
        .expect_err("an idle peer must hit the deadline");
    assert!(matches!(error, ProtocolError::Timeout));
    drop(writer);
}

/// One frame is one write: the length prefix is patched into space the buffer already reserved.
#[tokio::test]
async fn one_frame_reaches_the_socket_in_one_write() {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    use tokio::io::AsyncWrite;

    #[derive(Default)]
    struct CountingWriter {
        writes: usize,
        bytes: Vec<u8>,
    }

    impl AsyncWrite for CountingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.writes += 1;
            self.bytes.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    let limits = FrameLimits {
        max_frame_bytes: 4 * 1024,
        io_timeout: Duration::from_secs(1),
    };
    let request = RequestEnvelope::invoke(None, invocation());
    let mut writer = CountingWriter::default();
    write_frame(&mut writer, &request, limits)
        .await
        .expect("frame writes");

    assert_eq!(writer.writes, 1, "one frame must be one write syscall");
    let expected = serde_json::to_vec(&request).expect("request serializes");
    let length = u32::try_from(expected.len()).expect("bounded frame length");
    assert_eq!(&writer.bytes[..4], &length.to_be_bytes());
    assert_eq!(&writer.bytes[4..], &expected[..]);

    // The reserved prefix is not payload, so the bound still counts exactly the JSON bytes.
    let mut exact = CountingWriter::default();
    let value = json!({"v": "x".repeat(26)});
    let encoded = serde_json::to_vec(&value).expect("value serializes");
    write_frame(
        &mut exact,
        &value,
        FrameLimits {
            max_frame_bytes: encoded.len(),
            io_timeout: Duration::from_secs(1),
        },
    )
    .await
    .expect("a frame exactly at the bound is accepted");
    assert_eq!(exact.bytes.len(), encoded.len() + 4);
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
    let value = serde_json::to_value(RequestEnvelope::invoke(None, invocation()))
        .expect("request serializes");
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
            "apiVersion": "dekopon.dev/broker/v1alpha3",
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
fn public_drn_is_typed_optional_proposal_data_and_never_provider_input() {
    let mut request = invocation();
    request.secret_use = Some(SecretUseProposal::HttpBasic {
        secret: "drn:com.xrl:secret:test:api/password"
            .parse()
            .expect("canonical DRN"),
        username: "userA".to_owned(),
    });
    let value = serde_json::to_value(&request).expect("request serializes");
    assert_eq!(
        value["secretUse"]["secret"],
        "drn:com.xrl:secret:test:api/password"
    );
    assert_eq!(value["secretUse"]["kind"], "httpBasic");
    assert_eq!(value["input"], json!({"message": "hello"}));
    assert!(!value["input"].to_string().contains("drn:"));
    let encoded = value.to_string();
    assert!(
        !encoded.contains("password\":"),
        "no resolved password field: {encoded}"
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
        assert!(matches!(
            request.request,
            BrokerRequest::Capabilities { attestation: None }
        ));
        write_frame(
            &mut stream,
            &ResponseEnvelope::capabilities(
                Vec::new(),
                Vec::new(),
                "fixture-epoch".parse().expect("fixture epoch"),
            ),
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

/// The executed-or-not distinction the wire codes carry must survive a client-local failure.
///
/// A request that never left is safe to resubmit under a fresh invocation identifier; a request
/// whose response was lost is not, because the broker may have finished a non-idempotent external
/// effect and replay rejection keys on the identifier a retry would replace.
#[cfg(unix)]
#[tokio::test]
async fn framing_failures_keep_the_executed_or_not_distinction() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    use tokio::net::UnixListener;

    use super::{
        BrokerClient, ClientError, ERROR_BROKER_UNAVAILABLE, ERROR_OUTCOME_UNAUDITED, ExchangePhase,
    };

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

    // A broker that reads the complete proposal and then dies before answering is exactly the
    // shape of the hazard: the work may have run, and nothing on this side can tell.
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept client fixture");
        read_frame::<_, RequestEnvelope>(&mut stream, limits)
            .await
            .expect("server decodes the proposal");
    });
    let client = BrokerClient::new(&socket, uid, limits).expect("valid client limits");
    let lost = client
        .invoke(None, invocation())
        .await
        .expect_err("a lost response must fail");
    server.await.expect("server fixture exits");
    assert!(
        matches!(
            &lost,
            ClientError::Protocol {
                phase: ExchangePhase::Response,
                ..
            }
        ),
        "expected a response-phase failure, got {lost}"
    );
    assert!(lost.may_have_executed());

    // Serialization stops at the bound, so nothing was delivered and nothing ran.
    let listener = UnixListener::bind(directory.path().join("unread.sock")).expect("bind fixture");
    let unread = directory.path().join("unread.sock");
    std::fs::set_permissions(&unread, std::fs::Permissions::from_mode(0o600))
        .expect("make fixture socket private");
    let tight = FrameLimits {
        max_frame_bytes: 64,
        io_timeout: Duration::from_secs(1),
    };
    let client = BrokerClient::new(&unread, uid, tight).expect("valid client limits");
    let oversized = client
        .invoke(None, invocation())
        .await
        .expect_err("an oversized proposal must fail");
    drop(listener);
    assert!(
        matches!(
            &oversized,
            ClientError::Protocol {
                phase: ExchangePhase::Request,
                ..
            }
        ),
        "expected a request-phase failure, got {oversized}"
    );
    assert!(!oversized.may_have_executed());
    // The bounded framing detail reaches Display, where a model and an operator both read it. It
    // names byte counts and never the socket path.
    let rendered = oversized.to_string();
    assert!(rendered.contains("maximum is 64"), "rendered {rendered}");
    assert!(!rendered.contains("unread.sock"), "rendered {rendered}");

    // The same distinction the broker spends two stable wire codes on.
    assert!(
        ClientError::Remote {
            code: ERROR_OUTCOME_UNAUDITED.to_owned(),
            message: "audit append failed after execution".to_owned(),
        }
        .may_have_executed()
    );
    assert!(
        !ClientError::Remote {
            code: ERROR_BROKER_UNAVAILABLE.to_owned(),
            message: "nothing executed".to_owned(),
        }
        .may_have_executed()
    );
    assert!(!ClientError::ConnectTimeout.may_have_executed());
}

/// A rejected inventory must name the agent and the bound, not just fail.
#[test]
fn inventory_and_usage_validation_name_the_offending_agent_and_bound() {
    let inventory = AgentInventory {
        agents: vec![ReportedAgent {
            id: "reviewer".parse().expect("valid agent"),
            description: "Reviews pull requests".to_owned(),
            enabled: true,
            model_class: None,
            providers: vec!["gh".parse().expect("valid provider")],
            capabilities: vec![ReportedAgentCapability {
                id: "gh.pull-request.read".parse().expect("valid capability"),
                provider: "gh".parse().expect("valid provider"),
                permissions: Vec::new(),
            }],
        }],
        truncated: false,
    };
    assert_eq!(inventory.validate(), Ok(()));

    let mut oversized = inventory.clone();
    oversized.agents[0].description = "x".repeat(MAX_REPORTED_TEXT_BYTES + 1);
    let error = oversized
        .validate()
        .expect_err("an oversized description is rejected");
    assert_eq!(
        error,
        InventoryError::TextTooLong {
            agent: "reviewer".parse().expect("valid agent"),
            field: "description",
            bytes: MAX_REPORTED_TEXT_BYTES + 1,
            maximum: MAX_REPORTED_TEXT_BYTES,
        }
    );
    let rendered = error.to_string();
    assert!(rendered.contains("reviewer"), "rendered {rendered}");
    assert!(
        rendered.contains(&MAX_REPORTED_TEXT_BYTES.to_string()),
        "rendered {rendered}"
    );
    // The bound is named, the operator-authored text that broke it is not.
    assert!(!rendered.contains("xxxx"), "rendered {rendered}");

    let mut undeclared = inventory.clone();
    undeclared.agents[0].capabilities[0].provider = "slack".parse().expect("valid provider");
    assert_eq!(
        undeclared
            .validate()
            .expect_err("a capability may not name an undeclared provider"),
        InventoryError::UndeclaredProvider {
            agent: "reviewer".parse().expect("valid agent"),
            capability: "gh.pull-request.read".parse().expect("valid capability"),
            provider: "slack".parse().expect("valid provider"),
        }
    );

    let mut duplicated = inventory.clone();
    duplicated.agents.push(inventory.agents[0].clone());
    assert_eq!(
        duplicated.validate().expect_err("duplicate agents fail"),
        InventoryError::DuplicateAgent {
            agent: "reviewer".parse().expect("valid agent"),
        }
    );

    let mut providers = inventory;
    providers.agents[0].providers = (0..=MAX_REPORTED_AGENT_PROVIDERS)
        .map(|index| {
            format!("gh{index}")
                .parse()
                .expect("generated provider identifier")
        })
        .collect();
    assert_eq!(
        providers
            .validate()
            .expect_err("a provider list past its bound fails"),
        InventoryError::TooMany {
            agent: "reviewer".parse().expect("valid agent"),
            collection: "providers",
            count: MAX_REPORTED_AGENT_PROVIDERS + 1,
            maximum: MAX_REPORTED_AGENT_PROVIDERS,
        }
    );

    assert_eq!(
        ModelUsageReport::default()
            .validate()
            .expect_err("an empty delta fails"),
        UsageReportError::ModelCalls {
            count: 0,
            maximum: MAX_REPORTED_MODEL_CALLS,
        }
    );
    assert_eq!(
        ModelUsageReport {
            model_calls: 1,
            output_unreported_calls: 2,
            ..ModelUsageReport::default()
        }
        .validate()
        .expect_err("more missing calls than calls fails"),
        UsageReportError::UnreportedCalls {
            field: "output",
            count: 2,
            calls: 1,
        }
    );
    assert_eq!(
        ModelUsageReport {
            model_calls: 1,
            input_tokens: MAX_REPORTED_TOKENS + 1,
            ..ModelUsageReport::default()
        }
        .validate()
        .expect_err("an oversized token count fails"),
        UsageReportError::Tokens {
            field: "input",
            count: MAX_REPORTED_TOKENS + 1,
            maximum: MAX_REPORTED_TOKENS,
        }
    );
}

/// One version identifier, three renderings, nothing keeping them equal but this.
#[test]
fn protocol_version_constant_wire_form_and_display_agree() {
    assert_eq!(
        serde_json::to_value(ProtocolVersion::V1Alpha3).expect("version serializes"),
        json!(PROTOCOL_VERSION)
    );
    assert_eq!(ProtocolVersion::V1Alpha3.to_string(), PROTOCOL_VERSION);
    assert_eq!(
        serde_json::from_value::<ProtocolVersion>(json!(PROTOCOL_VERSION))
            .expect("version decodes"),
        ProtocolVersion::V1Alpha3
    );
    assert_eq!(
        serde_json::to_value(RequestEnvelope::capabilities(None))
            .expect("envelope serializes")
            .get("apiVersion"),
        Some(&json!(PROTOCOL_VERSION))
    );
}

#[test]
fn chat_scope_turn_and_attestation_debug_are_fully_redacted_and_bounded() {
    let scope = ChatScopeClaim {
        transport: "scientist-slack".parse().expect("transport"),
        kind: ChatTransportKind::Slack,
        channel: "c0123abc".to_owned(),
        conversation: "c0123abc:1712345678.000100".to_owned(),
    };
    let session = Attestation::for_chat(
        "slack.t0123abc.u9xyz".parse().expect("subject"),
        "reviewer".parse().expect("agent"),
        scope.clone(),
    );
    let attestation = session.bound_to("invoke-chat".parse().expect("invocation"));
    let turn = DeliveredTurnRequest {
        id: "invoke-chat".parse().expect("invocation"),
        trace: "trace-chat".parse().expect("trace"),
        trace_parent: None,
        delivery: DeliveryIdentity::Slack {
            channel: "c0123abc".to_owned(),
            timestamp: "1712345678.000100".to_owned(),
        },
        user: "private user sentinel".to_owned(),
        assistant: "private assistant sentinel".to_owned(),
    };
    assert!(scope.is_bounded() && turn.is_bounded());
    for rendered in [
        format!("{scope:?}"),
        format!("{session:?}"),
        format!("{attestation:?}"),
        format!("{turn:?}"),
    ] {
        assert_eq!(rendered.matches("[REDACTED]").count(), 1);
        for sentinel in ["c0123abc", "u9xyz", "reviewer", "private"] {
            assert!(!rendered.contains(sentinel));
        }
    }

    let oversized = ChatScopeClaim {
        channel: "x".repeat(257),
        ..scope
    };
    assert!(!oversized.is_bounded());
    let oversized_turn = DeliveredTurnRequest {
        user: "x".repeat(64 * 1024),
        assistant: "y".to_owned(),
        ..turn
    };
    assert!(!oversized_turn.is_bounded());
}

#[test]
fn delivery_identities_are_typed_canonical_and_bound_to_scope() {
    let slack = ChatScopeClaim {
        transport: "scientist-slack".parse().expect("transport"),
        kind: ChatTransportKind::Slack,
        channel: "c0123abc".to_owned(),
        conversation: "c0123abc:1712345678.000100".to_owned(),
    };
    assert!(
        DeliveryIdentity::Slack {
            channel: "c0123abc".to_owned(),
            timestamp: "1712345678.000101".to_owned(),
        }
        .is_canonical_for(&slack)
    );
    for timestamp in ["01712345678.000101", "1712345678.1", "171234567.000101"] {
        assert!(
            !DeliveryIdentity::Slack {
                channel: "c0123abc".to_owned(),
                timestamp: timestamp.to_owned(),
            }
            .is_canonical_for(&slack)
        );
    }
    assert!(
        !DeliveryIdentity::Discord {
            channel: "123".to_owned(),
            message: "456".to_owned(),
        }
        .is_canonical_for(&slack)
    );

    let discord = ChatScopeClaim {
        transport: "discord".parse().expect("transport"),
        kind: ChatTransportKind::Discord,
        channel: "123".to_owned(),
        conversation: "123".to_owned(),
    };
    for (channel, message) in [("0123", "456"), ("123", "0"), ("123", "0456")] {
        assert!(
            !DeliveryIdentity::Discord {
                channel: channel.to_owned(),
                message: message.to_owned(),
            }
            .is_canonical_for(&discord)
        );
    }

    let telegram = ChatScopeClaim {
        transport: "tg".parse().expect("transport"),
        kind: ChatTransportKind::Telegram,
        channel: "-1001".to_owned(),
        conversation: "-1001:topic:42".to_owned(),
    };
    assert!(
        DeliveryIdentity::Telegram {
            chat: "-1001".to_owned(),
            topic: Some("42".to_owned()),
            message: "7".to_owned(),
        }
        .is_canonical_for(&telegram)
    );
    assert!(
        !DeliveryIdentity::Telegram {
            chat: "-1001".to_owned(),
            topic: None,
            message: "7".to_owned(),
        }
        .is_canonical_for(&telegram)
    );
    for (chat, topic, message) in [
        ("-01001", Some("42"), "7"),
        ("-1001", Some("042"), "7"),
        ("-1001", Some("42"), "07"),
        ("-1001", Some("9223372036854775808"), "7"),
        ("-1001", Some("42"), "9223372036854775808"),
    ] {
        assert!(
            !DeliveryIdentity::Telegram {
                chat: chat.to_owned(),
                topic: topic.map(str::to_owned),
                message: message.to_owned(),
            }
            .is_canonical_for(&telegram)
        );
    }

    let telegram_max = ChatScopeClaim {
        transport: "tg".parse().expect("transport"),
        kind: ChatTransportKind::Telegram,
        channel: i64::MIN.to_string(),
        conversation: format!("{}:topic:{}", i64::MIN, i64::MAX),
    };
    assert!(
        DeliveryIdentity::Telegram {
            chat: i64::MIN.to_string(),
            topic: Some(i64::MAX.to_string()),
            message: i64::MAX.to_string(),
        }
        .is_canonical_for(&telegram_max),
        "every Telegram identifier representable by the gateway remains canonical"
    );

    let whatsapp = ChatScopeClaim {
        transport: "support-whatsapp".parse().expect("transport"),
        kind: ChatTransportKind::Whatsapp,
        channel: "123:456:16034700182".to_owned(),
        conversation: "123:456:16034700182".to_owned(),
    };
    let whatsapp_delivery = DeliveryIdentity::Whatsapp {
        waba: "123".to_owned(),
        phone_number: "456".to_owned(),
        message: "wamid.delivery/a+b=".to_owned(),
    };
    assert!(whatsapp_delivery.is_canonical_for(&whatsapp));
    let wire = serde_json::to_value(&whatsapp_delivery).expect("serialize WhatsApp delivery");
    assert_eq!(
        serde_json::from_value::<DeliveryIdentity>(wire).expect("deserialize WhatsApp delivery"),
        whatsapp_delivery
    );
    for (waba, phone_number, message) in [
        ("0123", "456", "wamid.delivery"),
        ("123", "0456", "wamid.delivery"),
        ("999", "456", "wamid.delivery"),
        ("123", "999", "wamid.delivery"),
        ("123", "456", ""),
    ] {
        assert!(
            !DeliveryIdentity::Whatsapp {
                waba: waba.to_owned(),
                phone_number: phone_number.to_owned(),
                message: message.to_owned(),
            }
            .is_canonical_for(&whatsapp)
        );
    }

    let local = ChatScopeClaim {
        transport: "dev".parse().expect("transport"),
        kind: ChatTransportKind::Local,
        channel: "conversation".to_owned(),
        conversation: "conversation".to_owned(),
    };
    let local_identity = |boot_nonce: &str, connection, sequence| DeliveryIdentity::Local {
        transport: "dev".parse().expect("transport"),
        conversation: "conversation".to_owned(),
        boot_nonce: boot_nonce.to_owned(),
        connection,
        sequence,
    };
    assert!(local_identity("0123456789abcdef0123456789abcdef", 1, 1).is_canonical_for(&local));
    for identity in [
        local_identity("0123456789abcdef0123456789abcdeg", 1, 1),
        local_identity("0123456789abcdef0123456789abcdef", 0, 1),
        local_identity("0123456789abcdef0123456789abcdef", 1, 0),
    ] {
        assert!(!identity.is_canonical_for(&local));
    }
}

#[test]
fn delivered_turn_strings_are_rejected_during_deserialization_at_their_field_bound() {
    let document = serde_json::json!({
        "id": "turn-bound",
        "trace": "trace-bound",
        "traceParent": null,
        "delivery": {
            "kind": "slack",
            "channel": "c0123abc",
            "timestamp": "1712345678.000100"
        },
        "user": "x".repeat(64 * 1024 + 1),
        "assistant": "answer"
    });
    assert!(serde_json::from_value::<DeliveredTurnRequest>(document).is_err());

    let scope = serde_json::json!({
        "transport": "scientist-slack",
        "kind": "slack",
        "channel": "c0123abc",
        "conversation": "x".repeat(257)
    });
    assert!(serde_json::from_value::<ChatScopeClaim>(scope).is_err());

    let delivery = serde_json::json!({
        "kind": "telegram",
        "chat": "-1001",
        "topic": "7".repeat(257),
        "message": "9"
    });
    assert!(serde_json::from_value::<DeliveryIdentity>(delivery).is_err());
    for (topic, message) in [("9223372036854775808", "9"), ("7", "9223372036854775808")] {
        assert!(
            serde_json::from_value::<DeliveryIdentity>(serde_json::json!({
                "kind": "telegram",
                "chat": "-1001",
                "topic": topic,
                "message": message
            }))
            .is_err(),
            "Telegram max+1 must fail during wire decoding"
        );
    }
}

#[cfg(unix)]
mod broker_socket_discovery {
    use std::path::PathBuf;

    use crate::{BrokerSocketDiscovery, BrokerSocketTier};

    fn every_tier_supplied() -> BrokerSocketDiscovery {
        BrokerSocketDiscovery::new(
            Some(PathBuf::from("/explicit/broker.sock")),
            Some(PathBuf::from("/environment/broker.sock")),
            Some(PathBuf::from("/run/user/501")),
            Some(PathBuf::from("/home/dekopon")),
        )
    }

    #[test]
    fn explicit_wins_over_every_other_tier() {
        let resolved = every_tier_supplied().resolve().expect("a tier applies");
        assert_eq!(resolved.path(), PathBuf::from("/explicit/broker.sock"));
        assert_eq!(resolved.tier(), BrokerSocketTier::Explicit);
    }

    #[test]
    fn environment_wins_over_the_derived_tiers() {
        let discovery = BrokerSocketDiscovery::new(
            None,
            Some(PathBuf::from("/environment/broker.sock")),
            Some(PathBuf::from("/run/user/501")),
            Some(PathBuf::from("/home/dekopon")),
        );
        let resolved = discovery.resolve().expect("a tier applies");
        assert_eq!(resolved.path(), PathBuf::from("/environment/broker.sock"));
        assert_eq!(resolved.tier(), BrokerSocketTier::Environment);
    }

    #[test]
    fn xdg_runtime_dir_derives_the_documented_suffix() {
        let discovery = BrokerSocketDiscovery::new(
            None,
            None,
            Some(PathBuf::from("/run/user/501")),
            Some(PathBuf::from("/home/dekopon")),
        );
        let resolved = discovery.resolve().expect("a tier applies");
        assert_eq!(
            resolved.path(),
            PathBuf::from("/run/user/501/dekopon/broker.sock")
        );
        assert_eq!(resolved.tier(), BrokerSocketTier::XdgRuntimeDir);
    }

    #[test]
    fn home_derives_the_documented_suffix() {
        let discovery =
            BrokerSocketDiscovery::new(None, None, None, Some(PathBuf::from("/home/dekopon")));
        let resolved = discovery.resolve().expect("a tier applies");
        assert_eq!(
            resolved.path(),
            PathBuf::from("/home/dekopon/.local/run/dekopon/broker.sock")
        );
        assert_eq!(resolved.tier(), BrokerSocketTier::Home);
    }

    #[test]
    fn no_tier_resolves_to_none_rather_than_a_guess() {
        assert!(
            BrokerSocketDiscovery::new(None, None, None, None)
                .resolve()
                .is_none()
        );
    }

    #[test]
    fn tier_labels_are_stable() {
        assert_eq!(BrokerSocketTier::Explicit.label(), "explicit");
        assert_eq!(BrokerSocketTier::Environment.label(), "environment");
        assert_eq!(BrokerSocketTier::XdgRuntimeDir.label(), "xdg-runtime-dir");
        assert_eq!(BrokerSocketTier::Home.label(), "home");
        assert_eq!(BrokerSocketTier::Home.to_string(), "home");
    }
}

/// One operation per verb, with the attestation as a field rather than an operation of its own.
///
/// The `operation` tag is the compatibility seam, so what each verb is spelled on the wire — and
/// that a subject-only claim, a chat claim and no claim at all reach the *same* tag — is the part
/// that has to be pinned rather than inferred.
#[test]
fn every_verb_is_one_operation_whatever_attestation_accompanies_it() {
    let turn = DeliveredTurnRequest {
        id: "invoke-chat".parse().expect("valid invocation fixture"),
        trace: "trace-chat".parse().expect("valid trace fixture"),
        trace_parent: None,
        delivery: DeliveryIdentity::Slack {
            channel: "c0123abc".to_owned(),
            timestamp: "1712345678.000100".to_owned(),
        },
        user: "hello".to_owned(),
        assistant: "hi".to_owned(),
    };
    let unattested = Attestation::for_subject(subject(), agent());
    let chat = Attestation::for_chat(subject(), agent(), scope());
    for (expected, envelope) in [
        ("capabilities", RequestEnvelope::capabilities(None)),
        (
            "capabilities",
            RequestEnvelope::capabilities(Some(unattested.clone())),
        ),
        (
            "capabilities",
            RequestEnvelope::capabilities(Some(chat.clone())),
        ),
        (
            "resolveCommand",
            RequestEnvelope {
                api_version: ProtocolVersion::V1Alpha3,
                request: BrokerRequest::ResolveCommand {
                    attestation: None,
                    word: "memory".to_owned(),
                    argv: Vec::new(),
                },
            },
        ),
        (
            "resolveCommand",
            RequestEnvelope {
                api_version: ProtocolVersion::V1Alpha3,
                request: BrokerRequest::ResolveCommand {
                    attestation: Some(chat.clone()),
                    word: "memory".to_owned(),
                    argv: vec!["recent".to_owned()],
                },
            },
        ),
        (
            "runCommand",
            RequestEnvelope::run_command(None, "memory".to_owned(), Vec::new(), None),
        ),
        (
            "runCommand",
            RequestEnvelope::run_command(
                Some(chat.clone()),
                "memory".to_owned(),
                vec!["search".to_owned(), "-".to_owned()],
                Some("piped".to_owned()),
            ),
        ),
        ("invoke", RequestEnvelope::invoke(None, invocation())),
        (
            "invoke",
            RequestEnvelope::invoke(Some(unattested.bound_to(invocation().id)), invocation()),
        ),
        (
            "invoke",
            RequestEnvelope::invoke(Some(chat.bound_to(invocation().id)), invocation()),
        ),
        (
            "recordDeliveredTurn",
            RequestEnvelope::record_delivered_turn(chat.bound_to(turn.id.clone()), turn),
        ),
        (
            "publishModelUsage",
            RequestEnvelope::publish_model_usage(ModelUsageReport {
                model_calls: 1,
                ..ModelUsageReport::default()
            }),
        ),
    ] {
        let encoded = serde_json::to_value(&envelope).expect("envelope serializes");
        assert_eq!(
            encoded["request"]["operation"],
            json!(expected),
            "{encoded}"
        );
        assert_eq!(
            serde_json::from_value::<RequestEnvelope>(encoded.clone()).expect("envelope decodes"),
            envelope,
            "{encoded}"
        );
    }
}

/// The version seam refuses a mixed pair in both directions, loudly, before anything is authorized.
///
/// The previous protocol spelled the attestation shape into the operation tag — `capabilitiesFor`,
/// `invokeForChat` — so a broker of this version reading an older client's frame would otherwise
/// have to guess. It does not: the `apiVersion` fails first, and the retired tags fail after it.
#[test]
fn the_previous_protocol_version_and_its_retired_operation_tags_both_fail_to_decode() {
    let previous = json!({
        "apiVersion": "dekopon.dev/broker/v1alpha1",
        "request": {"operation": "capabilities"}
    });
    assert!(serde_json::from_value::<RequestEnvelope>(previous).is_err());
    assert!(
        serde_json::from_value::<ResponseEnvelope>(json!({
            "apiVersion": "dekopon.dev/broker/v1alpha1",
            "response": {"type": "acknowledged"}
        }))
        .is_err()
    );

    for retired in [
        json!({"operation": "capabilitiesFor", "subject": "slack.t0123abc.u9xyz", "agent": "reviewer"}),
        json!({"operation": "capabilitiesForChat", "claim": {}}),
        json!({"operation": "resolveCommandForChat", "claim": {}, "word": "memory", "argv": []}),
        json!({"operation": "invokeFor", "invocation": {}, "attestation": {}}),
        json!({"operation": "invokeForChat", "invocation": {}, "attestation": {}}),
        json!({"operation": "recordDeliveredTurnForChat", "turn": {}, "attestation": {}}),
    ] {
        assert!(
            serde_json::from_value::<RequestEnvelope>(json!({
                "apiVersion": PROTOCOL_VERSION,
                "request": retired,
            }))
            .is_err(),
            "{retired} decoded under the current version"
        );
    }
}

/// A claim is bound to the proposal it travels with, and carries no identifier without one.
///
/// The binding is redundant inside a single frame by construction, which is the point: it is
/// defense in depth against a future refactor separating the claim from the proposal, and the
/// client fills it in so a caller cannot build a frame whose two halves disagree.
#[test]
fn a_claim_binds_to_its_proposal_and_holds_no_identifier_without_one() {
    let identifier = invocation().id;
    let unbound = Attestation::for_chat(subject(), agent(), scope());
    assert!(unbound.invocation.is_none());
    assert!(!unbound.binds(&identifier));

    let bound = unbound.bound_to(identifier.clone());
    assert!(bound.binds(&identifier));
    assert!(!bound.binds(&"invoke-other".parse().expect("valid invocation fixture")));
    assert_eq!(bound.subject, unbound.subject);
    assert_eq!(bound.scope, unbound.scope);

    // Structural bounds are checked before any grant is consulted, and a subject-only claim has no
    // scope to bound.
    assert!(Attestation::for_subject(subject(), agent()).is_well_formed());
    assert!(unbound.is_well_formed());
    assert!(
        !Attestation::for_chat(
            subject(),
            agent(),
            ChatScopeClaim {
                channel: "x".repeat(257),
                ..scope()
            }
        )
        .is_well_formed()
    );
}

/// Recording stays reachable only through its own operation, whatever attestation accompanies one.
///
/// `RecordDeliveredTurn` is the only variant that carries a [`DeliveredTurnRequest`] at all, and
/// every variant is `deny_unknown_fields`, so a proposal cannot smuggle a turn into `invoke` and
/// an attestation cannot promote one.
#[test]
fn recording_is_reachable_only_through_its_own_operation() {
    let turn = json!({
        "id": "invoke-chat",
        "trace": "trace-chat",
        "delivery": {"kind": "slack", "channel": "c0123abc", "timestamp": "1712345678.000100"},
        "user": "hello",
        "assistant": "hi",
    });
    for smuggled in [
        json!({"operation": "invoke", "invocation": {
            "id": "invoke-chat", "capability": "echo.echo", "trace": "trace-chat", "input": {},
        }, "turn": turn.clone()}),
        json!({"operation": "resolveCommand", "word": "memory", "argv": [], "turn": turn.clone()}),
        json!({"operation": "runCommand", "word": "memory", "argv": [], "turn": turn.clone()}),
        json!({"operation": "capabilities", "turn": turn}),
    ] {
        assert!(
            serde_json::from_value::<RequestEnvelope>(json!({
                "apiVersion": PROTOCOL_VERSION,
                "request": smuggled,
            }))
            .is_err(),
            "{smuggled} decoded a delivered turn onto an operation that must not carry one"
        );
    }
}

/// The piped value is one optional field on the run frame, absent when nothing was piped, so a
/// bare run is the same frame with or without a `stdin` key and a piped one carries the text.
#[test]
fn a_run_command_frame_omits_an_absent_piped_value() {
    let bare =
        RequestEnvelope::run_command(None, "probe".to_owned(), vec!["--help".to_owned()], None);
    let encoded = serde_json::to_value(&bare).expect("envelope serializes");
    assert_eq!(encoded["request"]["operation"], json!("runCommand"));
    assert!(encoded["request"].get("stdin").is_none(), "{encoded}");
    assert_eq!(
        serde_json::from_value::<RequestEnvelope>(encoded).expect("envelope decodes"),
        bare
    );

    let piped = RequestEnvelope::run_command(
        None,
        "probe".to_owned(),
        vec!["upper".to_owned(), "-".to_owned()],
        Some("hello".to_owned()),
    );
    let encoded = serde_json::to_value(&piped).expect("envelope serializes");
    assert_eq!(encoded["request"]["stdin"], json!("hello"), "{encoded}");
    assert_eq!(
        serde_json::from_value::<RequestEnvelope>(encoded).expect("envelope decodes"),
        piped
    );
}

/// Every answer a guest can give travels intact under its own tag, so a script sees exactly what
/// the upstream tool would have printed and a decline keeps its stable code.
#[test]
fn a_command_run_response_round_trips_each_outcome() {
    for (expected, result) in [
        (
            "proposed",
            CommandRunOutcome::Proposed {
                capability: "cli-probe.upper"
                    .parse::<CapabilityId>()
                    .expect("valid capability fixture"),
                input: json!({"text": "hello"}),
            },
        ),
        (
            "rendered",
            CommandRunOutcome::Rendered {
                stdout: "Usage: probe <COMMAND>\n".to_owned(),
                stderr: String::new(),
                status: 0,
            },
        ),
        (
            "failed",
            CommandRunOutcome::Failed {
                error: ComponentFailure {
                    code: "usage".to_owned(),
                    message: "no input was piped for -".to_owned(),
                },
            },
        ),
    ] {
        let envelope = ResponseEnvelope::command_run(result);
        let encoded = serde_json::to_value(&envelope).expect("envelope serializes");
        assert_eq!(
            encoded["response"]["type"],
            json!("commandRun"),
            "{encoded}"
        );
        assert_eq!(
            encoded["response"]["result"]["outcome"],
            json!(expected),
            "{encoded}"
        );
        assert_eq!(
            serde_json::from_value::<ResponseEnvelope>(encoded.clone()).expect("envelope decodes"),
            envelope,
            "{encoded}"
        );
    }
}

/// A rendered answer crosses the socket as the guest produced it: the client hands back the help
/// page and the status the provider chose, never a decline dressed as one.
#[cfg(unix)]
#[tokio::test]
async fn a_run_command_exchange_decodes_a_rendered_answer() {
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
    let rendered = CommandRunOutcome::Rendered {
        stdout: "Usage: probe <COMMAND>\n".to_owned(),
        stderr: String::new(),
        status: 0,
    };
    let answer = rendered.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept client fixture");
        let request = read_frame::<_, RequestEnvelope>(&mut stream, limits)
            .await
            .expect("server decodes request");
        assert_eq!(
            request.request,
            BrokerRequest::RunCommand {
                attestation: None,
                word: "probe".to_owned(),
                argv: vec!["--help".to_owned()],
                stdin: None,
            }
        );
        write_frame(&mut stream, &ResponseEnvelope::command_run(answer), limits)
            .await
            .expect("server writes response");
    });

    let client = BrokerClient::new(&socket, uid, limits).expect("valid client limits");
    let outcome = client
        .run_command(None, "probe".to_owned(), vec!["--help".to_owned()], None)
        .await
        .expect("authenticated exchange succeeds");
    assert_eq!(outcome, rendered);
    server.await.expect("server fixture exits");
}

/// An oversized piped value stops at the frame ceiling on this side: nothing is written, the
/// failure sits in the request phase, and it names the bound rather than the socket.
#[cfg(unix)]
#[tokio::test]
async fn an_oversized_piped_value_is_refused_before_it_leaves_the_client() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    use tokio::net::UnixListener;

    use super::{BrokerClient, ClientError, ExchangePhase};

    let directory = tempfile::tempdir().expect("create socket fixture directory");
    let socket = directory.path().join("unread.sock");
    let listener = UnixListener::bind(&socket).expect("bind broker fixture");
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .expect("make fixture socket private");
    let uid = std::fs::metadata(&socket).expect("socket metadata").uid();
    let tight = FrameLimits {
        max_frame_bytes: 64,
        io_timeout: Duration::from_secs(1),
    };
    let client = BrokerClient::new(&socket, uid, tight).expect("valid client limits");
    let refused = client
        .run_command(
            None,
            "probe".to_owned(),
            vec!["upper".to_owned(), "-".to_owned()],
            Some("x".repeat(256)),
        )
        .await
        .expect_err("an oversized piped value must fail");
    drop(listener);
    assert!(
        matches!(
            &refused,
            ClientError::Protocol {
                phase: ExchangePhase::Request,
                source: ProtocolError::FrameTooLarge { .. },
            }
        ),
        "expected a request-phase frame bound, got {refused}"
    );
    assert!(!refused.may_have_executed());
    let rendered = refused.to_string();
    assert!(rendered.contains("maximum is 64"), "rendered {rendered}");
    assert!(!rendered.contains("unread.sock"), "rendered {rendered}");
}

/// A refused attested inspection reaches the client as an opaque failure, never as an empty list.
///
/// Answering with an empty capability list would tell an ungranted caller that the subject is
/// mapped. The client must therefore surface the stable failure code and nothing else — no
/// capability, no command word, no memory surface.
#[cfg(unix)]
#[tokio::test]
async fn a_refused_attested_surface_is_a_stable_failure_rather_than_an_empty_answer() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    use tokio::net::UnixListener;

    use super::{BrokerClient, ClientError, ERROR_UNAUTHENTICATED};

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
        let BrokerRequest::Capabilities {
            attestation: Some(claim),
        } = request.request
        else {
            panic!("an attested surface must reach the one capabilities operation");
        };
        assert!(claim.scope.is_some());
        assert!(claim.invocation.is_none());
        write_frame(
            &mut stream,
            &ResponseEnvelope::error(ERROR_UNAUTHENTICATED, "attestation refused"),
            limits,
        )
        .await
        .expect("server writes refusal");
    });

    let client = BrokerClient::new(&socket, uid, limits).expect("valid client limits");
    let refused = client
        .session_surface(Some(Attestation::for_chat(subject(), agent(), scope())))
        .await
        .expect_err("a refused attestation is not an answer");
    server.await.expect("server fixture exits");
    let ClientError::Remote { code, .. } = refused else {
        panic!("expected a stable remote refusal, got {refused}");
    };
    assert_eq!(code, ERROR_UNAUTHENTICATED);
}
