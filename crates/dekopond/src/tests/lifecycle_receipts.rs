use super::*;
use dekopon_harness::{accounting::CallOutcome, history::DeliveryDisposition};

#[derive(Clone, Copy)]
enum SendResult {
    Accepted,
    Partial,
    Failed,
    Unknown,
}
struct ReceiptReplier(SendResult);
impl ChatReplier for ReceiptReplier {
    fn reply(
        &self,
        _: ReplyTarget,
        _: OutboundReply,
    ) -> BoxFuture<'_, Result<DeliveryReceipt, TransportError>> {
        Box::pin(async move {
            match self.0 {
                SendResult::Accepted => Ok(DeliveryReceipt::new("confirmed")),
                SendResult::Partial => Err(TransportError::PartialDelivery),
                SendResult::Failed => Err(TransportError::Service {
                    code: "ratelimited".into(),
                }),
                SendResult::Unknown => Err(TransportError::Response),
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn generation_delivery_and_terminal_checkpoint_receipts_remain_distinct() {
    for (generated, send, disposition, delivery) in [
        (
            true,
            SendResult::Accepted,
            DeliveryDisposition::Accepted {
                text: "generated answer".into(),
            },
            "accepted",
        ),
        (
            false,
            SendResult::Accepted,
            DeliveryDisposition::Accepted {
                text: FAILURE_REPLY.into(),
            },
            "accepted",
        ),
        (
            true,
            SendResult::Partial,
            DeliveryDisposition::Partial,
            "partial",
        ),
        (
            true,
            SendResult::Failed,
            DeliveryDisposition::Failed,
            "failed",
        ),
        (
            true,
            SendResult::Unknown,
            DeliveryDisposition::Unknown,
            "unknown",
        ),
    ] {
        let directory = temporary();
        let mut responses = vec![memory_surface_response(); if generated { 3 } else { 2 }];
        responses.push(ResponseEnvelope::invocation(record_result(
            InvocationOutcome::Succeeded,
            None,
        )));
        let (broker, mut observed) = stub_broker(directory.path(), responses).await;
        let models = ModelScript::scripted([generated.then(|| {
            let mut turn = answer("generated answer");
            turn.usage = Some(dekopon_model::model::ModelUsage::from_fields([
                Some(11),
                Some(3),
                Some(7),
                None,
                Some(18),
            ]));
            turn
        })]);
        let runner = runner(broker, models.clone(), 4);
        let bound = persistent_route(model_config(), window());
        let inbound = message("receipt matrix");
        run_session(
            runner.clone(),
            bound.clone(),
            inbound.clone(),
            Arc::new(ReceiptReplier(send)),
        )
        .await;
        let seed = session_seed(&runner, &bound, &inbound, memory_surface_response());
        let [record] = seed.history.turns() else {
            panic!("one independent job survives")
        };
        assert_eq!(
            record.generated.as_deref(),
            generated.then_some("generated answer")
        );
        assert_eq!(record.delivery, disposition);
        let checkpoint = dekopon_harness::checkpoint::memory_checkpoints()
            .load(&record.job)
            .unwrap();
        assert!(checkpoint.finalized);
        assert_eq!(checkpoint.record, *record);
        let tracker = checkpoint.state.accounting;
        assert!(tracker.finalized);
        assert_eq!(tracker.delivery, delivery);
        assert_eq!(
            tracker.generation,
            if generated {
                CallOutcome::Succeeded
            } else {
                CallOutcome::Failed
            }
        );
        assert_eq!(tracker.calls.len(), 1);
        assert_eq!(tracker.calls[0].attempts.len(), 1);
        assert_eq!(
            tracker.totals().cumulative.input.complete(),
            generated.then_some(11)
        );
        assert_eq!(
            tracker.totals().cumulative.output.complete(),
            generated.then_some(7)
        );
        assert_eq!(
            tracker.totals().cumulative.reasoning_output.complete(),
            None,
            "missing usage stays unknown after delivery"
        );
        assert_surface_checks(&mut observed, if generated { 3 } else { 2 });
        if generated && matches!(send, SendResult::Accepted) {
            assert!(matches!(
                observed.try_recv().unwrap().request,
                BrokerRequest::RecordDeliveredTurn { .. }
            ));
        }
        assert!(
            observed.try_recv().is_err(),
            "only completely accepted generated answers reach durable memory"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_retains_unknown_execution_when_the_entire_text_window_is_evicted() {
    let directory = temporary();
    let socket = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let (entered, dispatched) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut entered = Some(entered);
        let mut released = Some(released);
        for index in 0..8 {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let request: RequestEnvelope = read_frame(&mut stream, FrameLimits::default())
                .await
                .unwrap();
            let response = if index == 5 {
                assert!(matches!(request.request, BrokerRequest::Invoke { .. }));
                entered.take().unwrap().send(()).unwrap();
                released.take().unwrap().await.unwrap();
                ResponseEnvelope::error("outcome-unaudited", "fixture execution result unknown")
            } else {
                assert!(matches!(
                    request.request,
                    BrokerRequest::Capabilities {
                        attestation: Some(_),
                        ..
                    }
                ));
                listings(1, &["echo.echo"]).remove(0)
            };
            write_frame(&mut stream, &response, FrameLimits::default())
                .await
                .unwrap();
        }
    });
    let broker = ResolvedBroker {
        socket_path: socket,
        server_uid: crate::current_uid(),
        frame: FrameLimits::default(),
    };
    let models = ModelScript::new([script_call("echo.echo")]);
    let surface = Arc::new(RecordingSurface::default());
    let mut runner = runner(broker, models.clone(), 4);
    Arc::get_mut(&mut runner)
        .unwrap()
        .activities
        .insert("dev".into(), surface.clone());
    let mut tiny = window();
    tiny.limits.max_bytes = 1;
    let bound = persistent_route(model_config(), tiny);
    let mut inbound = message("oversized request before Stop");
    inbound.activity = Some(ActivityTarget::Slack {
        channel_id: "C1".into(),
        thread_ts: "1700000000.000001".into(),
        message_ts: "1700000000.000001".into(),
        initiator_user_id: "U1".into(),
    });
    let job = tokio::spawn(run_session(
        runner.clone(),
        bound.clone(),
        inbound.clone(),
        surface.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(10), dispatched)
        .await
        .unwrap()
        .unwrap();
    let mut controls = tokio::task::JoinSet::new();
    crate::stop_session(
        &runner,
        &mut controls,
        crate::transport::SessionStop {
            transport: inbound.transport.clone(),
            conversation_id: inbound.conversation_id.clone(),
            subject: subject(),
        },
    );
    while let Some(result) = controls.join_next().await {
        result.unwrap();
    }
    release.send(()).unwrap();
    job.await.unwrap();
    let seed = session_seed(
        &runner,
        &bound,
        &inbound,
        listings(1, &["echo.echo"]).remove(0),
    );
    assert!(seed.history.is_empty(), "the tiny window evicted all text");
    assert!(
        seed.history.has_unknown_work(),
        "Stop cannot erase unresolved execution with the text"
    );
    let replier = Arc::new(RecordingReplier::default());
    inbound.activity = None;
    inbound.text = "follow up".into();
    run_session(runner.clone(), bound, inbound, replier.clone()).await;
    server.await.unwrap();
    assert_eq!(
        models.requests(),
        1,
        "unknown work fences later inference and execution"
    );
    assert_eq!(replier.replies(), [FAILURE_REPLY]);
    assert!(
        surface
            .events()
            .contains(&format!("reply:{}", crate::session::STOPPED_REPLY))
    );
    assert!(
        !surface.events().contains(&format!("reply:{FAILURE_REPLY}")),
        "Stop suppresses the original job answer"
    );
}
