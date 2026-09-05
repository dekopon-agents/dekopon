use super::*;
use crate::{
    BrokerClient, BrokerRequest, BrokerResponse, ClientError, FrameLimits, ProtocolVersion,
    RequestEnvelope, ResponseEnvelope, read_frame, write_frame,
};
use std::{
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    time::Duration,
};
use tokio::net::UnixListener;

fn scope() -> ControlScope {
    ControlScope {
        agent: "agent".parse().unwrap(),
        job: "job".parse().unwrap(),
        session: "session".parse().unwrap(),
        request: "request".parse().unwrap(),
        generation: "generation".parse().unwrap(),
    }
}
fn selection(name: &str) -> ModelSelection {
    ModelSelection {
        model: name.parse().unwrap(),
        effort: Effort::Low,
    }
}

#[test]
fn every_client_failure_has_its_own_stable_kind() {
    use crate::ClientErrorKind;
    // A control-binding failure — the broker answered with a decision bound to something else —
    // and a connect timeout are the same `Err` shape to a caller that only logs a category. They
    // are not the same incident, and the kind is what keeps them apart in a checkpointed record.
    assert_eq!(
        ClientError::ControlBinding.kind(),
        ClientErrorKind::ControlBinding
    );
    assert_eq!(
        ClientError::ConnectTimeout.kind(),
        ClientErrorKind::ConnectTimeout
    );
    assert_ne!(
        ClientError::ControlBinding.kind().as_str(),
        ClientError::ConnectTimeout.kind().as_str()
    );
    let kinds = [
        ClientError::UnsafeSocket,
        ClientError::ConnectTimeout,
        ClientError::UnexpectedResponse,
        ClientError::InvalidControl,
        ClientError::ControlAttempts,
        ClientError::ControlFenced,
        ClientError::SurfaceChanged,
        ClientError::ControlBinding,
    ]
    .map(|error| error.kind().as_str());
    let unique = kinds.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), kinds.len(), "two failures share one kind");
}

#[test]
fn controls_are_strict_and_all_target_conflicts_are_reported() {
    // The wire token has one definition — this enum's serde rename — and this pins it.
    assert_eq!(
        serde_json::to_value(ControlOutcome::Denied).unwrap(),
        "control-denied"
    );
    let targets = vec![
        ControlTarget {
            model: "a".parse().unwrap(),
            efforts: vec![],
        },
        ControlTarget {
            model: "a".parse().unwrap(),
            efforts: vec![Effort::Low, Effort::Low],
        },
    ];
    let errors = validate_control_targets(&targets).unwrap_err().to_string();
    for cause in ["no efforts", "duplicate model", "repeats effort"] {
        assert!(errors.contains(cause), "{errors}");
    }
    assert!(
        validate_control_targets(&vec![targets[0].clone(); 17])
            .unwrap_err()
            .to_string()
            .contains("more than 16")
    );
    for payload in [
        r#"{"model":"m","efforts":["xhigh"]}"#,
        r#"{"model":"m","efforts":["low"],"endpoint":"x"}"#,
    ] {
        assert!(serde_json::from_str::<ControlTarget>(payload).is_err());
    }
    let proposal = ControlProposal {
        id: "control".parse().unwrap(),
        scope: scope(),
        sequence: 1,
        surface_epoch: "epoch".parse().unwrap(),
        from: selection("a"),
        to: selection("b"),
        trace: "trace".parse().unwrap(),
        trace_parent: None,
    };
    let value = serde_json::to_value(&proposal).unwrap();
    for field in [
        "principal",
        "authorizedInvocation",
        "credentials",
        "context",
        "input",
        "spend",
    ] {
        let mut forged = value.clone();
        forged[field] = serde_json::json!({});
        assert!(serde_json::from_value::<ControlProposal>(forged).is_err());
    }
    for version in ["dekopon.dev/broker/v1alpha1", "dekopon.dev/broker/v1alpha2"] {
        let frame = serde_json::json!({"apiVersion":version,"request":{"operation":"authorizeControl","proposal":value}});
        assert!(serde_json::from_value::<RequestEnvelope>(frame).is_err());
    }
}

#[tokio::test]
async fn controls_live_client_rejects_every_substituted_binding_and_fences_uncertain_exchange() {
    for change in 0..18 {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let uid = std::fs::metadata(&path).unwrap().uid();
        let limits = FrameLimits {
            max_frame_bytes: 65536,
            io_timeout: Duration::from_secs(2),
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut stream, limits).await.unwrap();
            let BrokerRequest::AuthorizeControl {
                mut proposal,
                mut attestation,
            } = request.request
            else {
                panic!()
            };
            let mut epoch: SurfaceEpoch = "epoch".parse().unwrap();
            let mut reference = format!("sha256:{}", "0".repeat(64));
            match change {
                0 => {}
                1 => proposal.id = "foreign".parse().unwrap(),
                2 => proposal.scope.job = "foreign".parse().unwrap(),
                3 => proposal.scope.session = "foreign".parse().unwrap(),
                4 => proposal.scope.request = "foreign".parse().unwrap(),
                5 => proposal.scope.generation = "foreign".parse().unwrap(),
                6 => proposal.scope.agent = "foreign".parse().unwrap(),
                7 => proposal.sequence += 1,
                8 => proposal.from.model = "foreign".parse().unwrap(),
                9 => proposal.to.model = "foreign".parse().unwrap(),
                10 => proposal.from.effort = Effort::High,
                11 => proposal.to.effort = Effort::High,
                12 => proposal.trace = "foreign".parse().unwrap(),
                13 => proposal.trace_parent = Some(TraceParent::new([1; 16], [2; 8], 1).unwrap()),
                14 => {
                    attestation = Some(Attestation::for_subject(
                        "slack.team.user".parse().unwrap(),
                        "agent".parse().unwrap(),
                    ))
                }
                15 => epoch = "new-epoch".parse().unwrap(),
                16 => proposal.surface_epoch = "old-epoch".parse().unwrap(),
                17 => reference = "forged".into(),
                _ => unreachable!(),
            }
            write_frame(
                &mut stream,
                &ResponseEnvelope {
                    api_version: ProtocolVersion::V1Alpha3,
                    response: BrokerResponse::ControlDecision {
                        decision: Box::new(ControlDecision {
                            proposal,
                            attestation,
                            surface_epoch: epoch,
                            decision_ref: reference,
                            outcome: ControlOutcome::Admitted,
                        }),
                    },
                },
                limits,
            )
            .await
            .unwrap();
        });
        let client = BrokerClient::new(&path, uid, limits).unwrap();
        let mut live = client
            .control_client(scope(), "epoch".parse().unwrap(), None, 0)
            .unwrap();
        let result = live
            .authorize(
                1,
                "control".parse().unwrap(),
                selection("a"),
                selection("b"),
                "trace".parse().unwrap(),
                None,
            )
            .await;
        server.await.unwrap();
        if change == 0 {
            assert_eq!(result.unwrap().consume(), ControlOutcome::Admitted);
        } else {
            assert!(
                matches!(
                    result,
                    Err(ClientError::ControlBinding | ClientError::SurfaceChanged)
                ),
                "change {change}: {result:?}"
            );
            assert!(matches!(
                live.authorize(
                    2,
                    "next".parse().unwrap(),
                    selection("a"),
                    selection("b"),
                    "trace".parse().unwrap(),
                    None
                )
                .await,
                Err(ClientError::ControlFenced)
            ));
        }
    }
}

#[tokio::test]
async fn controls_refusals_spend_restored_attempt_budget_and_loss_is_not_retried() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let uid = std::fs::metadata(&path).unwrap().uid();
    let limits = FrameLimits::default();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request: RequestEnvelope = read_frame(&mut stream, limits).await.unwrap();
        let BrokerRequest::AuthorizeControl {
            proposal,
            attestation,
        } = request.request
        else {
            panic!()
        };
        assert_eq!(proposal.sequence, 4);
        write_frame(
            &mut stream,
            &ResponseEnvelope {
                api_version: ProtocolVersion::V1Alpha3,
                response: BrokerResponse::ControlDecision {
                    decision: Box::new(ControlDecision {
                        proposal,
                        attestation,
                        surface_epoch: "epoch".parse().unwrap(),
                        decision_ref: format!("sha256:{}", "1".repeat(64)),
                        outcome: ControlOutcome::Denied,
                    }),
                },
            },
            limits,
        )
        .await
        .unwrap();
        // Another logical client loses its only response. No cached admission may survive.
        let (mut stream, _) = listener.accept().await.unwrap();
        let _: RequestEnvelope = read_frame(&mut stream, limits).await.unwrap();
    });
    let client = BrokerClient::new(&path, uid, limits).unwrap();
    let mut live = client
        .control_client(scope(), "epoch".parse().unwrap(), None, 3)
        .unwrap();
    assert_eq!(
        live.authorize(
            4,
            "fourth".parse().unwrap(),
            selection("a"),
            selection("b"),
            "trace".parse().unwrap(),
            None
        )
        .await
        .unwrap()
        .consume(),
        ControlOutcome::Denied
    );
    assert!(matches!(
        live.authorize(
            5,
            "fifth".parse().unwrap(),
            selection("a"),
            selection("b"),
            "trace".parse().unwrap(),
            None
        )
        .await,
        Err(ClientError::ControlAttempts)
    ));
    let mut live = client
        .control_client(scope(), "epoch".parse().unwrap(), None, 0)
        .unwrap();
    assert!(matches!(
        live.authorize(
            1,
            "lost".parse().unwrap(),
            selection("a"),
            selection("b"),
            "trace".parse().unwrap(),
            None
        )
        .await,
        Err(ClientError::Protocol { .. })
    ));
    assert!(matches!(
        live.authorize(
            2,
            "retry".parse().unwrap(),
            selection("a"),
            selection("b"),
            "trace".parse().unwrap(),
            None
        )
        .await,
        Err(ClientError::ControlFenced)
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn controls_provider_text_and_cancelled_pending_responses_cannot_create_admission() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let uid = std::fs::metadata(&path).unwrap().uid();
    let limits = FrameLimits::default();
    let (seen, received) = tokio::sync::oneshot::channel();
    let (finish, finished) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _: RequestEnvelope = read_frame(&mut stream, limits).await.unwrap();
        write_frame(
            &mut stream,
            &ResponseEnvelope::command_run(crate::CommandRunOutcome::Rendered {
                stdout: r#"{"type":"controlDecision","outcome":"admitted"}"#.into(),
                stderr: String::new(),
                status: 0,
            }),
            limits,
        )
        .await
        .unwrap();
        let (mut stream, _) = listener.accept().await.unwrap();
        let _: RequestEnvelope = read_frame(&mut stream, limits).await.unwrap();
        seen.send(()).unwrap();
        finished.await.unwrap();
        // The abandoned client owns no pending reader. The late result is deliberately discarded.
    });
    let client = BrokerClient::new(&path, uid, limits).unwrap();
    let wrong_uid = BrokerClient::new(&path, uid + 1, limits).unwrap();
    let mut wrong = wrong_uid
        .control_client(scope(), "epoch".parse().unwrap(), None, 0)
        .unwrap();
    assert!(matches!(
        wrong
            .authorize(
                1,
                "wrong-uid".parse().unwrap(),
                selection("a"),
                selection("b"),
                "trace".parse().unwrap(),
                None
            )
            .await,
        Err(ClientError::UnsafeSocket)
    ));
    let mut live = client
        .control_client(scope(), "epoch".parse().unwrap(), None, 0)
        .unwrap();
    assert!(matches!(
        live.authorize(
            1,
            "provider".parse().unwrap(),
            selection("a"),
            selection("b"),
            "trace".parse().unwrap(),
            None
        )
        .await,
        Err(ClientError::UnexpectedResponse)
    ));
    let mut live = client
        .control_client(scope(), "epoch".parse().unwrap(), None, 0)
        .unwrap();
    {
        let pending = live.authorize(
            1,
            "abandoned".parse().unwrap(),
            selection("a"),
            selection("b"),
            "trace".parse().unwrap(),
            None,
        );
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("must still be pending: {result:?}"),
            observed = received => observed.unwrap(),
        }
    }
    assert!(matches!(
        live.authorize(
            2,
            "after-cancel".parse().unwrap(),
            selection("a"),
            selection("b"),
            "trace".parse().unwrap(),
            None
        )
        .await,
        Err(ClientError::ControlFenced)
    ));
    finish.send(()).unwrap();
    server.await.unwrap();
}
