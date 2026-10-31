use super::*;
use dekopon_agent::wake::WakeId;
use dekopon_broker_protocol::ChatTransportKind;
use dekopon_process::JobId;

pub(super) fn call() -> AssistantTurn {
    AssistantTurn::new(
        None,
        vec![ModelToolCall {
            id: "reason-call".into(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: "wake_reason".to_owned(),
                arguments: "{}".to_owned(),
            },
        }],
        None,
    )
}

fn result(prompt: Vec<(String, String)>) -> Value {
    let (_, text) = prompt
        .into_iter()
        .rev()
        .find(|(role, _)| role == "tool")
        .expect("the model receives the tool result");
    serde_json::from_str(&text).expect("wake_reason returns JSON")
}

#[tokio::test(flavor = "multi_thread")]
async fn wake_reason_never_names_a_subject_or_conversation() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        principal_listings(&["first", "ana", "ana"]),
    )
    .await;
    let models = InterruptibleModel::new([
        answer("first"),
        call(),
        answer("follow-up"),
        call(),
        answer("wake"),
    ]);
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
    let route = route(model_config());
    let driver = Arc::new(RecordingDriver::default());
    let mut first = wakes::slack_message("first");
    first.subject =
        dekopon_core::AttestedSubject::one("drn:slack:user:t0123abc:u1".parse().expect("subject"))
            .unwrap();
    first.conversation.container = Some("t0123abc".into());
    first.conversation.id = "d0123abc".into();
    first.conversation.thread = Some("1750000000.000001".into());
    let mut follow = first.clone();
    follow.subject =
        dekopon_core::AttestedSubject::one("drn:slack:user:t0123abc:u2".parse().expect("subject"))
            .unwrap();
    follow.message_id = MessageId::Native("1750000000.000002".into());
    follow.text = "follow-up".into();
    let held = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        first,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    models.wait_until_entered().await;
    run_session(
        Arc::clone(&runner),
        route.clone(),
        follow.clone(),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    models.release();
    held.await.unwrap();
    assert_eq!(
        result(models.prompt(2)),
        json!({
            "trigger":"chat", "transportKind":"slack", "principal":"ana",
            "admission":"followUp", "steers":0,
        })
    );
    follow.message_id = MessageId::Wake {
        id: WakeId(42),
        notice: "scheduled".into(),
        origin: crate::wake::WakeOrigin::Scheduled,
    };
    run_session(runner, route, follow, driver).await;
    assert_eq!(
        result(models.prompt(4)),
        json!({
            "trigger":"wake", "wake":"w-42", "scheduledBy":"ana",
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wake_reason_is_offered_on_every_gateway_session() {
    let github: dekopon_core::github::GithubEvent = serde_json::from_value(json!({
        "delivery":"delivery-1", "event":"push",
        "repository":{"id":10,"fullName":"owner/repo"},
        "actor":{"id":1}, "kind":"none", "authorSet":{"actorLogin":"octocat"},
    }))
    .unwrap();
    for inspect in [false, true] {
        for id in [
            message("native").message_id,
            MessageId::Wake {
                id: WakeId(42),
                notice: "scheduled".into(),
                origin: crate::wake::WakeOrigin::Scheduled,
            },
            MessageId::Job {
                id: JobId::new(9),
                agent: "reviewer".parse().unwrap(),
            },
            MessageId::Github(Box::new(dekopon_core::github::GithubBatch::new(
                github.clone(),
            ))),
        ] {
            let directory = temporary();
            let (broker, _observed) =
                stub_broker(directory.path(), principal_listings(&["ana"])).await;
            let models = ModelScript::new([call(), answer("done")]);
            let mut route = route(model_config());
            route.inspect_agent_config = inspect;
            let mut inbound = message("why?");
            match &id {
                MessageId::Github(_) => {
                    inbound.subject =
                        dekopon_core::AttestedSubject::github_repository("owner", "repo", 1)
                            .unwrap();
                    inbound.transport_kind = ChatTransportKind::Github;
                    inbound.conversation.kind = ConversationKind::Thread;
                    inbound.conversation.container = Some("owner".into());
                    inbound.conversation.id = "owner.repo:repository".into();
                    inbound.conversation.thread = Some("repository".into());
                }
                MessageId::Native(_)
                | MessageId::Follow(_)
                | MessageId::Wake { .. }
                | MessageId::Job { .. } => {}
            }
            inbound.message_id = id;
            run_session(
                runner(broker, Arc::clone(&models), 1),
                route,
                inbound,
                Arc::new(RecordingDriver::default()),
            )
            .await;
            assert!(
                models
                    .tool_names(0)
                    .iter()
                    .any(|name| name == "wake_reason")
            );
            assert_eq!(
                models
                    .tool_names(0)
                    .iter()
                    .any(|name| name == AGENT_CONFIG_TOOL_NAME),
                inspect
            );
            let value = result(models.prompt(1));
            assert!(value.get("trigger").is_some());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wake_reason_counts_consumed_person_steers() {
    for steering in [config::Steering::Abort, config::Steering::Boundary] {
        let directory = temporary();
        let (broker, _observed) = stub_broker(directory.path(), principal_listings(&["ana"])).await;
        let turns = match steering {
            config::Steering::Abort => vec![call(), answer("done")],
            config::Steering::Boundary => vec![answer("draft"), call(), answer("done")],
        };
        let models = InterruptibleModel::new(turns);
        let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 1);
        let mut route = route(model_config());
        route.steering = steering;
        let driver = Arc::new(RecordingDriver::default());
        let held = tokio::spawn(run_session(
            Arc::clone(&runner),
            route.clone(),
            message("first"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        ));
        models.wait_until_entered().await;
        run_session(runner, route, message("also this"), driver).await;
        if steering == config::Steering::Boundary {
            models.release();
        }
        held.await.unwrap();
        assert_eq!(
            result(models.prompt(2)),
            json!({
                "trigger":"chat", "transportKind":"local", "principal":"ana",
                "admission":"new", "steers":1,
            })
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wake_reason_distinguishes_fresh_follow_messages_from_dequeued_admission() {
    let directory = temporary();
    let journal_dir = temporary();
    let (broker, _observed) = stub_broker(directory.path(), principal_listings(&["ana"])).await;
    let models = ModelScript::new([call(), answer("done")]);
    let mut runner =
        Arc::into_inner(runner(broker, Arc::clone(&models), 1)).expect("runner is uniquely owned");
    let journal = Arc::new(
        crate::journal::Journal::open(
            journal_dir.path(),
            Duration::from_secs(86400),
            Duration::from_secs(86400),
        )
        .unwrap(),
    );
    let table = Arc::new(crate::follow::FollowTable::load(journal).unwrap());
    let mut route = route(model_config());
    route.follow = true;
    let inbound = InboundMessage {
        message_id: MessageId::Follow("follow-id".into()),
        ..message("follow")
    };
    let key = crate::follow::follow_key(
        &route.transport,
        route.agent.as_str(),
        &route.conversation,
        &inbound.conversation.key(),
    );
    table.stay_and_write(&key, SystemTime::now()).unwrap();
    runner.follow = Some(table);
    run_session(
        Arc::new(runner),
        route,
        inbound,
        Arc::new(RecordingDriver::default()),
    )
    .await;
    assert_eq!(
        result(models.prompt(1)),
        json!({
            "trigger":"chat", "transportKind":"local", "principal":"ana", "admission":"new", "steers":0,
        })
    );
}
