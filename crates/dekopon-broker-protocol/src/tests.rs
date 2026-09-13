use std::time::Duration;

use dekopon_core::{CapabilityId, InvocationId, SecretUseProposal};
use serde_json::json;
use tokio::io::{AsyncWriteExt as _, duplex};

use super::{
    Attestation, BrokerRequest, ChatScopeClaim, ChatTransportKind, CommandRunOutcome,
    ComponentFailure, Conversation, ConversationKind, ConversationKindMatch, ConversationMatch,
    ConversationMatchProblem, DeliveredTurnRequest, DeliveryIdentity, FrameLimits,
    InvocationRequest, PROTOCOL_VERSION, ProtocolError, ProtocolVersion, RequestEnvelope,
    ResponseEnvelope, TraceParent, TraceParentError, read_frame, write_frame,
};

/// One conversation fixture, spelled the way a transport mints it.
fn conversation(
    kind: ConversationKind,
    container: Option<&str>,
    id: &str,
    thread: Option<&str>,
) -> Conversation {
    Conversation {
        kind,
        container: container.map(str::to_owned),
        id: id.to_owned(),
        thread: thread.map(str::to_owned),
    }
}

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
        conversation: conversation(
            ConversationKind::Channel,
            Some("t0123abc"),
            "c0123abc",
            Some("1712345678.000100"),
        ),
    }
}

/// A socket parent both sides of the socket rule accept.
///
/// `tempfile::tempdir` applies the process umask, which normally leaves the directory
/// world-traversable — a parent the broker refuses to bind under, and one this client refuses to
/// connect through however private the socket's own mode looks. Fixtures that expect an exchange
/// to happen start from a directory the broker could really have bound in.
#[cfg(unix)]
fn private_socket_directory() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("create socket fixture directory");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private socket fixture directory");
    directory
}

fn invocation() -> InvocationRequest {
    InvocationRequest {
        id: "invoke-test"
            .parse::<InvocationId>()
            .expect("valid invocation fixture"),
        capability: "echo.echo"
            .parse::<CapabilityId>()
            .expect("valid capability fixture"),
        trace_parent: SAMPLE_TRACE_PARENT
            .parse::<TraceParent>()
            .expect("valid traceparent fixture"),
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

/// `traceParent` is mandatory, and it carries the trace the audit record is correlated by.
///
/// It used to be an `Option` beside a separate Dekopon `trace` field, and a client that exported
/// no telemetry sent `null`. Both halves are gone: the broker sources its audit correlation from
/// this one field, so a request that omits it names no trace for the decision it is about to
/// cause, and a decode that accepted the omission would write exactly the record an operator
/// cannot find.
#[test]
fn invocation_request_requires_one_well_formed_trace_parent() {
    let complete = serde_json::to_value(invocation()).expect("request serializes");
    assert_eq!(
        complete.get("traceParent"),
        Some(&json!(SAMPLE_TRACE_PARENT))
    );
    assert!(
        complete.get("trace").is_none(),
        "the second identifier is gone"
    );

    let decoded =
        serde_json::from_value::<InvocationRequest>(complete.clone()).expect("request decodes");
    assert_eq!(decoded.trace_parent.to_string(), SAMPLE_TRACE_PARENT);
    assert_eq!(
        decoded.trace_parent.trace().to_string(),
        "4bf92f3577b34da6a3ce929d0e0e4736"
    );

    for rejected in [json!(null), json!("not-a-traceparent")] {
        let mut request = complete.clone();
        request
            .as_object_mut()
            .expect("request object")
            .insert("traceParent".to_owned(), rejected.clone());
        assert!(
            serde_json::from_value::<InvocationRequest>(request).is_err(),
            "accepted {rejected}"
        );
    }

    let mut omitted = complete;
    omitted
        .as_object_mut()
        .expect("request object")
        .remove("traceParent");
    assert!(serde_json::from_value::<InvocationRequest>(omitted).is_err());
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
            "apiVersion": "dekopon.dev/broker/v1alpha2",
            "request": {
                "operation": "invoke",
                "invocation": {
                    "id": "invoke-test",
                    "capability": "echo.echo",
                    "traceParent": SAMPLE_TRACE_PARENT,
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

#[cfg(unix)]
#[tokio::test]
async fn unix_client_authenticates_private_socket_and_response_variant() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    use tokio::net::UnixListener;

    use super::BrokerClient;

    let directory = private_socket_directory();
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

/// The executed-or-not distinction the wire codes carry must survive a client-local failure.
///
/// A request that never left is safe to resubmit under a fresh invocation identifier; a request
/// whose response was lost is not, because the broker may have finished a non-idempotent external
/// effect and suppresses no duplicate.
#[cfg(unix)]
#[tokio::test]
async fn framing_failures_keep_the_executed_or_not_distinction() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    use tokio::net::UnixListener;

    use super::{
        BrokerClient, ClientError, ERROR_BROKER_UNAVAILABLE, ERROR_OUTCOME_UNAUDITED, ExchangePhase,
    };

    let directory = private_socket_directory();
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

/// One version identifier, three renderings, nothing keeping them equal but this.
#[test]
fn protocol_version_constant_wire_form_and_display_agree() {
    assert_eq!(
        serde_json::to_value(ProtocolVersion::V1Alpha2).expect("version serializes"),
        json!(PROTOCOL_VERSION)
    );
    assert_eq!(ProtocolVersion::V1Alpha2.to_string(), PROTOCOL_VERSION);
    assert_eq!(
        serde_json::from_value::<ProtocolVersion>(json!(PROTOCOL_VERSION))
            .expect("version decodes"),
        ProtocolVersion::V1Alpha2
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
    let scope = scope();
    let session = Attestation::for_chat(
        "slack.t0123abc.u9xyz".parse().expect("subject"),
        "reviewer".parse().expect("agent"),
        scope.clone(),
    );
    let attestation = session.bound_to("invoke-chat".parse().expect("invocation"));
    let turn = DeliveredTurnRequest {
        id: "invoke-chat".parse().expect("invocation"),
        trace_parent: SAMPLE_TRACE_PARENT
            .parse()
            .expect("valid traceparent fixture"),
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
        format!("{:?}", scope.conversation),
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
        conversation: conversation(
            ConversationKind::Channel,
            Some("t0123abc"),
            &"x".repeat(257),
            None,
        ),
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
fn one_canonical_conversation_form_decides_every_transport() {
    let sender =
        |value: &str| -> dekopon_core::ExternalSubject { value.parse().expect("subject fixture") };
    let slack_user = sender("slack.t0123abc.u9xyz");
    let discord_user = sender("discord.578258790881951745");
    let telegram_user = sender("telegram.5551234");
    let whatsapp_user = sender("whatsapp.16034700182");

    // Every accepted row of the derivation table, with the key each one is filed under.
    for (transport, subject, conversation, key) in [
        (
            ChatTransportKind::Slack,
            &slack_user,
            conversation(
                ConversationKind::DirectMessage,
                Some("t0123abc"),
                "d0123abc",
                None,
            ),
            "d0123abc",
        ),
        (
            ChatTransportKind::Slack,
            &slack_user,
            conversation(
                ConversationKind::DirectMessage,
                Some("t0123abc"),
                "d0123abc",
                Some("1712345678.000100"),
            ),
            "d0123abc:1712345678.000100",
        ),
        // A multi-person DM is `G…` on older workspaces and `C…` on newer ones: both are just
        // lowercase tokens here, which is what F15 asked for.
        (
            ChatTransportKind::Slack,
            &slack_user,
            conversation(
                ConversationKind::GroupDirectMessage,
                Some("t0123abc"),
                "g0123abc",
                Some("1712345678.000100"),
            ),
            "g0123abc:1712345678.000100",
        ),
        (
            ChatTransportKind::Slack,
            &slack_user,
            conversation(
                ConversationKind::GroupDirectMessage,
                Some("t0123abc"),
                "c0999zzz",
                Some("1712345678.000100"),
            ),
            "c0999zzz:1712345678.000100",
        ),
        (
            ChatTransportKind::Slack,
            &slack_user,
            conversation(
                ConversationKind::Channel,
                Some("t0123abc"),
                "c0123abc",
                Some("1712345678.000100"),
            ),
            "c0123abc:1712345678.000100",
        ),
        (
            ChatTransportKind::Slack,
            &slack_user,
            conversation(
                ConversationKind::Thread,
                Some("t0123abc"),
                "c0123abc",
                Some("1712345600.000100"),
            ),
            "c0123abc:1712345600.000100",
        ),
        (
            ChatTransportKind::Discord,
            &discord_user,
            conversation(ConversationKind::DirectMessage, None, "123", None),
            "123",
        ),
        (
            ChatTransportKind::Discord,
            &discord_user,
            conversation(ConversationKind::Channel, Some("999"), "123", None),
            "123",
        ),
        (
            ChatTransportKind::Discord,
            &discord_user,
            conversation(ConversationKind::Thread, Some("999"), "123", Some("456")),
            "123:456",
        ),
        // The private chat id *is* the sender's user id, which is the correlation S25 pins.
        (
            ChatTransportKind::Telegram,
            &telegram_user,
            conversation(ConversationKind::DirectMessage, None, "5551234", None),
            "5551234",
        ),
        (
            ChatTransportKind::Telegram,
            &telegram_user,
            conversation(ConversationKind::DirectMessage, None, "5551234", Some("7")),
            "5551234:7",
        ),
        (
            ChatTransportKind::Telegram,
            &telegram_user,
            conversation(ConversationKind::Channel, None, "-1001", None),
            "-1001",
        ),
        (
            ChatTransportKind::Telegram,
            &telegram_user,
            conversation(ConversationKind::Thread, None, "-1001", Some("7")),
            "-1001:7",
        ),
        (
            ChatTransportKind::Whatsapp,
            &whatsapp_user,
            conversation(
                ConversationKind::DirectMessage,
                Some("123:456"),
                "16034700182",
                None,
            ),
            "16034700182",
        ),
        (
            ChatTransportKind::Local,
            &telegram_user,
            conversation(ConversationKind::DirectMessage, None, "dev", None),
            "dev",
        ),
        (
            ChatTransportKind::Local,
            &telegram_user,
            conversation(ConversationKind::Thread, Some("cli"), "dev.1", None),
            "dev.1",
        ),
    ] {
        assert!(
            conversation.is_canonical_for(transport, subject),
            "{transport} {conversation:?} is the form the transport mints"
        );
        assert_eq!(conversation.key(), key);
    }

    for (transport, subject, conversation, why) in [
        (
            ChatTransportKind::Slack,
            &slack_user,
            conversation(
                ConversationKind::Channel,
                Some("t0123abc"),
                "C0123ABC",
                Some("1712345678.000100"),
            ),
            "Slack ids are lowercase tokens",
        ),
        (
            ChatTransportKind::Slack,
            &slack_user,
            conversation(
                ConversationKind::Channel,
                Some("t0123abc"),
                "c0123abc",
                None,
            ),
            "a channel message always answers in a thread",
        ),
        (
            ChatTransportKind::Slack,
            &slack_user,
            conversation(
                ConversationKind::Channel,
                None,
                "c0123abc",
                Some("1712345678.000100"),
            ),
            "the Slack team is required",
        ),
        (
            ChatTransportKind::Slack,
            &slack_user,
            conversation(
                ConversationKind::DirectMessage,
                Some("t0123abc"),
                "d0123abc",
                Some("1712345678.1"),
            ),
            "a short fraction is not a Slack timestamp",
        ),
        (
            ChatTransportKind::Discord,
            &discord_user,
            conversation(ConversationKind::DirectMessage, Some("999"), "123", None),
            "a Discord direct message has no guild",
        ),
        (
            ChatTransportKind::Discord,
            &discord_user,
            conversation(ConversationKind::Channel, None, "123", None),
            "a guild channel has a guild",
        ),
        (
            ChatTransportKind::Discord,
            &discord_user,
            conversation(ConversationKind::Thread, Some("999"), "123", None),
            "a thread names the thread it answers in",
        ),
        (
            ChatTransportKind::Discord,
            &discord_user,
            conversation(ConversationKind::GroupDirectMessage, None, "123", None),
            "Discord group DMs are never routed",
        ),
        (
            ChatTransportKind::Discord,
            &discord_user,
            conversation(ConversationKind::Channel, Some("999"), "00123", None),
            "a snowflake carries no leading zero",
        ),
        (
            ChatTransportKind::Telegram,
            &telegram_user,
            conversation(ConversationKind::DirectMessage, None, "5559999", None),
            "a Telegram direct message is the sender's own chat",
        ),
        (
            ChatTransportKind::Telegram,
            &telegram_user,
            conversation(ConversationKind::DirectMessage, Some("t"), "5551234", None),
            "Telegram has no container",
        ),
        (
            ChatTransportKind::Telegram,
            &telegram_user,
            conversation(ConversationKind::Channel, None, "1001", None),
            "a Telegram group id is negative",
        ),
        (
            ChatTransportKind::Telegram,
            &telegram_user,
            conversation(ConversationKind::Thread, None, "-1001", Some("0")),
            "a topic id is a positive decimal",
        ),
        (
            ChatTransportKind::Whatsapp,
            &whatsapp_user,
            conversation(
                ConversationKind::DirectMessage,
                Some("123:456"),
                "16039999999",
                None,
            ),
            "a WhatsApp conversation is its sender",
        ),
        (
            ChatTransportKind::Whatsapp,
            &whatsapp_user,
            conversation(
                ConversationKind::DirectMessage,
                Some("123:456:16034700182"),
                "16034700182",
                None,
            ),
            "the 0.13 three-part channel is not a container",
        ),
        (
            ChatTransportKind::Whatsapp,
            &whatsapp_user,
            conversation(
                ConversationKind::Channel,
                Some("123:456"),
                "16034700182",
                None,
            ),
            "WhatsApp produces no channels",
        ),
        (
            ChatTransportKind::Local,
            &telegram_user,
            conversation(ConversationKind::DirectMessage, None, "DEV", None),
            "local scope values are lowercase",
        ),
        (
            ChatTransportKind::Local,
            &telegram_user,
            conversation(ConversationKind::DirectMessage, None, "dev", Some("1")),
            "the local transport has no threads",
        ),
        // Outside the wire bounds the grammar never runs: an unbounded part fails closed.
        (
            ChatTransportKind::Local,
            &telegram_user,
            conversation(
                ConversationKind::DirectMessage,
                None,
                &"b".repeat(257),
                None,
            ),
            "an unbounded id fails closed",
        ),
        (
            ChatTransportKind::Slack,
            &discord_user,
            conversation(
                ConversationKind::Channel,
                Some("t0123abc"),
                "c0123abc",
                Some("1712345678.000100"),
            ),
            "a Discord subject cannot have sent a Slack message",
        ),
    ] {
        assert!(
            !conversation.is_canonical_for(transport, subject),
            "{transport} {conversation:?}: {why}"
        );
    }
}

/// The kind word is one spelling: the YAML list entry, the Cedar string, and the trace attribute.
#[test]
fn every_conversation_kind_has_exactly_one_spelling() {
    for kind in [
        ConversationKind::DirectMessage,
        ConversationKind::GroupDirectMessage,
        ConversationKind::Channel,
        ConversationKind::Thread,
    ] {
        assert_eq!(
            serde_json::to_value(kind).expect("kind serializes"),
            json!(kind.as_str())
        );
        assert_eq!(
            serde_json::from_value::<ConversationKind>(json!(kind.as_str())).expect("kind decodes"),
            kind
        );
        assert_eq!(kind.to_string(), kind.as_str());
    }
}

/// A Discord thread is itself a channel; every other service threads inside one.
#[test]
fn the_api_channel_is_the_thread_only_where_a_thread_is_a_channel() {
    let discord = conversation(ConversationKind::Thread, Some("999"), "123", Some("456"));
    assert_eq!(discord.api_channel(ChatTransportKind::Discord), "456");
    let slack = conversation(
        ConversationKind::Thread,
        Some("t0123abc"),
        "c0123abc",
        Some("1712345678.000100"),
    );
    assert_eq!(slack.api_channel(ChatTransportKind::Slack), "c0123abc");
    assert_eq!(
        conversation(ConversationKind::Channel, Some("999"), "123", None)
            .api_channel(ChatTransportKind::Discord),
        "123"
    );
}

/// A selector reports every problem at once, and `kind` is a list or the word `any`.
#[test]
fn a_conversation_selector_reports_every_problem_at_once() {
    let bad = ConversationMatch {
        kind: ConversationKindMatch::Kinds(vec![
            ConversationKind::Channel,
            ConversationKind::Channel,
            ConversationKind::GroupDirectMessage,
        ]),
        container: Some("nine hundred".to_owned()),
        ids: Some(vec!["123:456".to_owned(), "00123".to_owned()]),
    };
    let problems = bad.validate(ChatTransportKind::Discord);
    assert_eq!(
        problems,
        vec![
            ConversationMatchProblem::DuplicateKind {
                kind: ConversationKind::Channel
            },
            ConversationMatchProblem::ImpossibleKind {
                kind: ConversationKind::GroupDirectMessage,
                transport: ChatTransportKind::Discord,
            },
            ConversationMatchProblem::NonCanonicalContainer {
                container: "nine hundred".to_owned()
            },
            ConversationMatchProblem::ThreadFormId {
                id: "123:456".to_owned()
            },
            ConversationMatchProblem::NonCanonicalId {
                id: "00123".to_owned()
            },
        ]
    );
    assert!(
        problems[3]
            .to_string()
            .contains("selectors name the parent conversation")
    );

    assert_eq!(
        ConversationMatch {
            kind: ConversationKindMatch::Kinds(Vec::new()),
            container: None,
            ids: Some(Vec::new()),
        }
        .validate(ChatTransportKind::Slack),
        vec![
            ConversationMatchProblem::EmptyKindList,
            ConversationMatchProblem::EmptyIds
        ]
    );
    assert_eq!(
        ConversationMatch {
            kind: ConversationKindMatch::Any,
            container: Some("t0123abc".to_owned()),
            ids: None,
        }
        .validate(ChatTransportKind::Telegram),
        vec![ConversationMatchProblem::ContainerNotSupported {
            transport: ChatTransportKind::Telegram
        }]
    );
    assert!(
        ConversationMatch {
            kind: ConversationKindMatch::Any,
            container: None,
            ids: None,
        }
        .validate(ChatTransportKind::Whatsapp)
        .is_empty()
    );

    // `kind: channel` reads as though it claimed the threads under the channel too, so the decoder
    // refuses it by name rather than accepting a narrower rule than it looks.
    let bare = serde_json::from_value::<ConversationMatch>(json!({"kind": "channel"}))
        .expect_err("a bare kind word is refused");
    assert!(bare.to_string().contains("kind: [channel]"), "{bare}");
    assert_eq!(
        serde_json::from_value::<ConversationMatch>(json!({"kind": "any"})).expect("any decodes"),
        ConversationMatch {
            kind: ConversationKindMatch::Any,
            container: None,
            ids: None
        }
    );
}

/// A selector names the parent; the kind list decides whether its threads come with it.
#[test]
fn a_selector_matches_kind_container_and_id_but_never_a_thread() {
    let selector = ConversationMatch {
        kind: ConversationKindMatch::Kinds(vec![
            ConversationKind::Channel,
            ConversationKind::Thread,
        ]),
        container: Some("999".to_owned()),
        ids: Some(vec!["123".to_owned()]),
    };
    assert!(selector.matches(&conversation(
        ConversationKind::Channel,
        Some("999"),
        "123",
        None
    )));
    assert!(selector.matches(&conversation(
        ConversationKind::Thread,
        Some("999"),
        "123",
        Some("456")
    )));
    assert!(!selector.matches(&conversation(
        ConversationKind::Thread,
        Some("999"),
        "456",
        Some("789")
    )));
    assert!(!selector.matches(&conversation(
        ConversationKind::Channel,
        Some("111"),
        "123",
        None
    )));
    assert!(!selector.matches(&conversation(
        ConversationKind::DirectMessage,
        None,
        "123",
        None
    )));

    let channels_only = ConversationMatch {
        kind: ConversationKindMatch::Kinds(vec![ConversationKind::Channel]),
        container: None,
        ids: None,
    };
    assert!(!channels_only.matches(&conversation(
        ConversationKind::Thread,
        Some("999"),
        "123",
        Some("456")
    )));
}

#[test]
fn delivery_identities_are_typed_canonical_and_bound_to_scope() {
    let slack = ChatScopeClaim {
        transport: "scientist-slack".parse().expect("transport"),
        kind: ChatTransportKind::Slack,
        conversation: conversation(
            ConversationKind::Channel,
            Some("t0123abc"),
            "c0123abc",
            Some("1712345678.000100"),
        ),
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
        conversation: conversation(ConversationKind::Channel, Some("999"), "123", None),
    };
    // A thread is its own channel on Discord, so the delivery names the thread and the
    // conversation names the parent.
    let discord_thread = ChatScopeClaim {
        transport: "discord".parse().expect("transport"),
        kind: ChatTransportKind::Discord,
        conversation: conversation(ConversationKind::Thread, Some("999"), "123", Some("456")),
    };
    assert!(
        DeliveryIdentity::Discord {
            channel: "456".to_owned(),
            message: "789".to_owned(),
        }
        .is_canonical_for(&discord_thread)
    );
    assert!(
        !DeliveryIdentity::Discord {
            channel: "123".to_owned(),
            message: "789".to_owned(),
        }
        .is_canonical_for(&discord_thread)
    );
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
        conversation: conversation(ConversationKind::Thread, None, "-1001", Some("42")),
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
        conversation: conversation(
            ConversationKind::Thread,
            None,
            &i64::MIN.to_string(),
            Some(&i64::MAX.to_string()),
        ),
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
        conversation: conversation(
            ConversationKind::DirectMessage,
            Some("123:456"),
            "16034700182",
            None,
        ),
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
        conversation: conversation(ConversationKind::DirectMessage, None, "conversation", None),
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
        "conversation": {"kind": "channel", "id": "x".repeat(257)}
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
        trace_parent: SAMPLE_TRACE_PARENT
            .parse()
            .expect("valid traceparent fixture"),
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
                api_version: ProtocolVersion::V1Alpha2,
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
                api_version: ProtocolVersion::V1Alpha2,
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
            "response": {"type": "capabilities", "capabilities": [], "commandWords": []}
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
                conversation: conversation(
                    ConversationKind::Channel,
                    Some("t0123abc"),
                    &"x".repeat(257),
                    None
                ),
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

    let directory = private_socket_directory();
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

    let directory = private_socket_directory();
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

    let directory = private_socket_directory();
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

#[cfg(unix)]
#[tokio::test]
async fn shared_socket_requires_protected_matching_parent_and_preserves_server_pinning() {
    use super::{BrokerClient, ClientError, validate_socket_path};
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broker.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
    let uid = std::fs::metadata(&path).unwrap().uid();
    for mode in [0o710, 0o750, 0o2710] {
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        validate_socket_path(&path, uid)
            .await
            .expect("protected shared socket");
    }
    for mode in [0o700, 0o740, 0o770, 0o711, 0o751, 0o1770] {
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(
            matches!(
                validate_socket_path(&path, uid).await,
                Err(ClientError::UnsafeSocket)
            ),
            "parent {mode:o}"
        );
    }
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o710)).unwrap();
    assert!(matches!(
        validate_socket_path(&path, uid.wrapping_add(1)).await,
        Err(ClientError::UnsafeSocket)
    ));
    let alias = directory.path().join("alias.sock");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    assert!(matches!(
        validate_socket_path(&alias, uid).await,
        Err(ClientError::UnsafeSocket)
    ));
    let parent_alias = directory.path().join("parent-alias");
    std::os::unix::fs::symlink(directory.path(), &parent_alias).unwrap();
    assert!(matches!(
        validate_socket_path(&parent_alias.join("broker.sock"), uid).await,
        Err(ClientError::UnsafeSocket)
    ));

    let limits = FrameLimits {
        max_frame_bytes: 4096,
        io_timeout: Duration::from_secs(1),
    };
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_frame::<_, RequestEnvelope>(&mut stream, limits)
            .await
            .unwrap();
        assert!(matches!(
            request.request,
            BrokerRequest::Capabilities { attestation: None }
        ));
        write_frame(
            &mut stream,
            &ResponseEnvelope::capabilities(Vec::new(), Vec::new()),
            limits,
        )
        .await
        .unwrap();
    });
    let client = BrokerClient::new(&path, uid, limits).unwrap();
    assert!(
        client
            .capabilities()
            .await
            .expect("owner still connects to group socket")
            .is_empty()
    );
    server.await.unwrap();
}

#[test]
fn retired_reporting_operations_are_refused() {
    for operation in ["publishAgentInventory", "publishModelUsage"] {
        let value = json!({"apiVersion": PROTOCOL_VERSION, "request": {"operation": operation}});
        assert!(serde_json::from_value::<RequestEnvelope>(value).is_err());
    }
}
