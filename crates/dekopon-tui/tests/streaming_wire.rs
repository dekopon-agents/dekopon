//! Always-run wire witness: console wrappers over the pinned broker client's real FD transport.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "tests spawn, join and drain freely"
)]

use dekopon_broker_protocol::{
    AssetEncoding, BrokerClient, BrokerResponse, DescriptorStream, FrameLimits, NewAsset,
    ProtocolVersion, ResponseEnvelope, RunOutcome,
};
use dekopon_core::SecretUseProposal;
use dekopon_shell::{CapabilityCallResult, CapabilityInvoker as _, CommandProposal, Streams};
use dekopon_tui::{
    record::{CallOutcome, RecordingInvoker, Sequence, SessionEvent},
    session::{LegHandle, assets::ConsoleAssets, open_agent, open_agent_with_assets},
};
use serde_json::{Value, json};
use std::{
    io::{Read as _, Write as _},
    os::unix::{fs::PermissionsExt as _, net::UnixStream},
    sync::Arc,
};

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "the existing end-to-end witness keeps setup, operation and assertions together"
)]
async fn returned_asset_crosses_shell_legs_only_within_one_agent_entry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("broker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server = tokio::spawn(async move {
        for (operation, agent) in [
            ("capabilities", "reviewer"),
            ("invoke", "reviewer"),
            ("capabilities", "reviewer"),
            ("run", "reviewer"),
            ("capabilities", "reviewer"),
            ("capabilities", "other-agent"),
        ] {
            let (stream, _) = listener.accept().await.unwrap();
            let mut wire = DescriptorStream::new(stream);
            let (frame, descriptors) = wire
                .read_frame::<Value>(FrameLimits::default())
                .await
                .unwrap();
            let request = &frame["request"];
            assert_eq!(request["operation"], operation);
            assert_eq!(request["attestation"]["agent"], agent);
            let mut attachment = None;
            let response = if operation == "capabilities" {
                assert!(descriptors.is_empty());
                ResponseEnvelope::capabilities(vec![serde_json::from_value(json!({
                    "provider":"probe", "capability": {"id":"probe.read", "description":"read bytes", "effect":"read-only", "risk":"Low", "inputSchema":{"type":"object"}}
                })).unwrap()], vec![], Default::default())
            } else if request["invocation"]["input"] == json!({"mode":"attach"}) {
                assert_eq!(descriptors.len(), 1, "first leg has stdout only");
                let mut file = tempfile::tempfile().unwrap();
                file.write_all(&[0, 1, 2, 255]).unwrap();
                attachment = Some(file);
                ResponseEnvelope::invocation(serde_json::from_value(json!({
                    "invocation":request["invocation"]["id"],
                    "decision":{"decisionId":"asset-test", "authorizedBy":"broker", "policyDigest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                    "outcome":"Succeeded"
                })).unwrap(), vec![NewAsset { descriptor:0, content_type:"image/heic".into(), encoding:AssetEncoding::Identity, bytes:4, sha256:String::new() }], vec![], vec![])
            } else {
                assert_eq!(request["word"], "probe");
                assert_eq!(request["argv"], json!([{"asset": 0}]));
                let rows = &request["assets"];
                assert_eq!(rows[0]["contentType"], "image/heic");
                assert_eq!(rows[0]["encoding"], "identity");
                assert_eq!(rows[0]["id"], 1);
                assert_eq!(descriptors.len(), 2, "asset and stdout, not inline bytes");
                let mut bytes = [0; 4];
                use std::os::unix::fs::FileExt as _;
                std::fs::File::from(descriptors.into_iter().next().unwrap())
                    .read_exact_at(&mut bytes, 0)
                    .unwrap();
                assert_eq!(bytes, [0, 1, 2, 255]);
                ResponseEnvelope {
                    api_version: ProtocolVersion::V1Alpha2,
                    response: BrokerResponse::Ran {
                        outcome: RunOutcome::Invoked {
                            capability: "probe.read".parse().unwrap(),
                            input: json!({"file":"chat-asset:1"}),
                            secret_use: None,
                            result: serde_json::from_value(json!({
                                "invocation":request["invocation"],
                                "decision":{"decisionId":"asset-test", "authorizedBy":"broker", "policyDigest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                                "outcome":"Failed", "exitStatus":141
                            })).unwrap(),
                            attached: vec![], removed: vec![], sent: vec![],
                        },
                    },
                }
            };
            let descriptors: Vec<std::os::fd::OwnedFd> =
                attachment.into_iter().map(Into::into).collect();
            use std::os::fd::AsFd as _;
            let borrowed: Vec<_> = descriptors.iter().map(|fd| fd.as_fd()).collect();
            wire.write_frame(&response, &borrowed, FrameLimits::default())
                .await
                .unwrap();
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "refused IDs must not reach broker"
        );
    });
    let client = BrokerClient::new(
        &socket,
        rustix::process::geteuid().as_raw(),
        FrameLimits::default(),
    )
    .unwrap();
    let subject: dekopon_core::ExternalSubject = "slack.t0123abc.u9xyz".parse().unwrap();
    let agent: dekopon_core::AgentId = "reviewer".parse().unwrap();
    let inventory = Arc::new(ConsoleAssets::default());
    let first = open_agent_with_assets(
        client.clone(),
        subject.clone(),
        agent.clone(),
        None,
        Arc::<ConsoleAssets>::clone(&inventory),
    )
    .await
    .unwrap();
    let invoke = |leg: dekopon_agent::BrokerLeg, input: Value| {
        let (stdout, _reader) = UnixStream::pair().unwrap();
        LegHandle(Arc::new(leg)).invoke(
            CommandProposal::new("probe.read", input, None),
            Streams {
                stdin: None,
                stdout: stdout.into(),
            },
            &dekopon_process::TreeContext::new(
                dekopon_process::Limits::default(),
                dekopon_process::CallBudget::new(4),
            ),
        )
    };
    let first_result = tokio::task::spawn_blocking(move || invoke(first, json!({"mode":"attach"})))
        .await
        .unwrap();
    assert!(
        matches!(first_result, CapabilityCallResult::SucceededWithStderr(note) if note.contains("chat-asset:1") && note.contains("image/heic"))
    );
    let second = open_agent_with_assets(
        client.clone(),
        subject.clone(),
        agent.clone(),
        None,
        inventory,
    )
    .await
    .unwrap();
    let run = |leg: dekopon_agent::BrokerLeg| {
        let (stdout, _reader) = UnixStream::pair().unwrap();
        let outcome = LegHandle(Arc::new(leg)).run(
            dekopon_shell::RunCall {
                word: "probe".to_owned(),
                argv: vec![dekopon_shell::Arg::Asset(
                    dekopon_core::AssetNumber::parse("chat-asset:1").unwrap(),
                )],
            },
            Streams {
                stdin: None,
                stdout: stdout.into(),
            },
            &dekopon_process::TreeContext::new(
                dekopon_process::Limits::default(),
                dekopon_process::CallBudget::new(4),
            ),
        );
        match outcome {
            dekopon_shell::RunResult::Invoked { result, .. }
            | dekopon_shell::RunResult::Returned(result)
            | dekopon_shell::RunResult::Uncertain(result) => result,
        }
    };
    let result = tokio::task::spawn_blocking(move || run(second))
        .await
        .unwrap();
    assert!(matches!(result, CapabilityCallResult::Exited { status, .. } if status.get() == 141));
    let reentered = open_agent_with_assets(
        client.clone(),
        subject.clone(),
        agent,
        None,
        Arc::new(ConsoleAssets::default()),
    )
    .await
    .unwrap();
    let other = open_agent_with_assets(
        client,
        subject,
        "other-agent".parse().unwrap(),
        None,
        Arc::new(ConsoleAssets::default()),
    )
    .await
    .unwrap();
    for stale in [reentered, other] {
        let result = tokio::task::spawn_blocking(move || run(stale))
            .await
            .unwrap();
        assert!(
            matches!(result, CapabilityCallResult::Denied { reason } if reason.contains("no attachment"))
        );
    }
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn console_forwards_attestation_secret_intent_and_descriptors_not_inline_stdio() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("broker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let secret = SecretUseProposal::HttpBearer {
        secret: "drn:com.xrl:secret:prod:api/token".parse().unwrap(),
    };
    let expected_secret = secret.clone();
    let server = tokio::spawn(async move {
        for operation in ["capabilities", "run"] {
            let (stream, _) = listener.accept().await.unwrap();
            let mut wire = DescriptorStream::new(stream);
            let (frame, mut descriptors) = wire
                .read_frame::<Value>(FrameLimits::default())
                .await
                .unwrap();
            let request = &frame["request"];
            assert_eq!(request["operation"], operation);
            assert_eq!(request["attestation"]["subject"], "slack.t0123abc.u9xyz");
            assert_eq!(request["attestation"]["agent"], "reviewer");
            let response = match operation {
                "capabilities" => {
                    assert!(descriptors.is_empty());
                    ResponseEnvelope::capabilities(vec![serde_json::from_value(json!({
                        "provider": "probe", "capability": { "id": "probe.upper", "description": "stream probe", "effect": "read-only", "risk": "Low", "inputSchema": {"type":"object"} }
                    })).unwrap()], vec!["probe".into()], [("probe".into(), "probe upper".into())].into())
                }
                "run" => {
                    assert_eq!(request["word"], "probe");
                    assert_eq!(request["argv"], json!([{"text":"upper"}]));
                    assert_eq!(request["streams"], json!({"stdin":true}));
                    assert_eq!(descriptors.len(), 2);
                    let streams = dekopon_broker_protocol::Streams::split_from(
                        &mut descriptors,
                        serde_json::from_value(request["streams"].clone()).unwrap(),
                    )
                    .unwrap();
                    assert!(descriptors.is_empty());
                    tokio::task::spawn_blocking(move || {
                        let mut text = String::new();
                        std::fs::File::from(streams.stdin.unwrap())
                            .read_to_string(&mut text)
                            .unwrap();
                        assert_eq!(text, "hello from stdin\n");
                        std::fs::File::from(streams.stdout)
                            .write_all(text.to_uppercase().as_bytes())
                            .unwrap();
                    })
                    .await
                    .unwrap();
                    ResponseEnvelope { api_version: dekopon_broker_protocol::ProtocolVersion::V1Alpha2, response: dekopon_broker_protocol::BrokerResponse::Ran { outcome: dekopon_broker_protocol::RunOutcome::Invoked {
                        capability: "probe.upper".parse().unwrap(),
                        input: json!({"mode":"upper"}),
                        secret_use: Some(expected_secret.clone()),
                        result: serde_json::from_value(json!({
                            "invocation": request["invocation"],
                            "decision": {"decisionId":"wire-test", "authorizedBy":"broker", "policyDigest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                            "outcome":"Failed", "exitStatus":141
                        })).unwrap(), attached: vec![], removed: vec![], sent: vec![],
                    } } }
                }
                _ => unreachable!(),
            };
            wire.write_frame(&response, &[], FrameLimits::default())
                .await
                .unwrap();
        }
    });
    let client = BrokerClient::new(
        &socket,
        rustix::process::geteuid().as_raw(),
        FrameLimits::default(),
    )
    .unwrap();
    let leg = open_agent(
        client,
        "slack.t0123abc.u9xyz".parse().unwrap(),
        "reviewer".parse().unwrap(),
        None,
    )
    .await
    .unwrap();
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || {
        let invoker = RecordingInvoker::new(LegHandle(Arc::new(leg)), events, Sequence::default());
        assert_eq!(invoker.command_word_help()["probe"], "probe upper");
        let (stdin, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"hello from stdin\n").unwrap();
        drop(writer);
        let (stdout, mut reader) = UnixStream::pair().unwrap();
        let result = invoker.run(dekopon_shell::RunCall { word: "probe".into(), argv: vec![dekopon_shell::Arg::Text("upper".into())] }, Streams { stdin: Some(stdin.into()), stdout: stdout.into() }, &dekopon_process::TreeContext::new(dekopon_process::Limits::default(), dekopon_process::CallBudget::new(4)));
        assert!(matches!(result, dekopon_shell::RunResult::Invoked { secret_use: Some(ref received), result: CapabilityCallResult::Exited { status, ref stderr }, .. } if *received == secret && status.get() == 141 && stderr.is_empty()));
        let mut output = String::new();
        reader.read_to_string(&mut output).unwrap();
        assert_eq!(output, "HELLO FROM STDIN\n", "observation must not consume provider stdout");
    }).await.unwrap();
    server.await.unwrap();
    let SessionEvent::Capability(call) = receiver.try_recv().unwrap() else {
        panic!("expected recorded terminal result")
    };
    assert_eq!(
        call.outcome,
        CallOutcome::Exited {
            status: 141,
            stderr: String::new()
        }
    );
    assert_eq!(call.payloads().len(), 1);
}
