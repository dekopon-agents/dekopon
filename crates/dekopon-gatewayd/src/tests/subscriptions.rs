use super::*;
use crate::{pull_request_links::PullRequestLinkContext, wake::Anchor};

fn subscription_anchor() -> Anchor {
    Anchor::from_inbound(
        &owned_slack_message("review", false),
        &route(model_config()).agent,
    )
    .unwrap()
    .as_space()
    .unwrap()
}

#[test]
fn a_dotted_repository_pr_link_parses_to_its_lowercase_full_name() {
    let links = PullRequestLinkContext::parse(
        "https://github.com/Dekopon-Agents/Dekopon-Agents.github.io/pull/18",
    )
    .expect("a dotted repository is a pull request link");
    assert_eq!(
        links.targets[0].repository.as_str(),
        "dekopon-agents/dekopon-agents.github.io"
    );
}

#[test]
fn the_sixty_fifth_subscription_is_refused_per_agent_across_channels() {
    let directory = temporary();
    let store = wakes::wake_store(directory.path());
    let anchor = subscription_anchor();
    let now = std::time::SystemTime::now();
    for number in 1..=64 {
        let target =
            PullRequestLinkContext::parse(&format!("https://github.com/acme/widget/pull/{number}"))
                .unwrap()
                .targets
                .remove(0);
        let mut message = owned_slack_message("review", false);
        message.conversation.id = if number % 2 == 0 { "cfirst" } else { "csecond" }.into();
        message.reply = ReplyTarget::Slack {
            channel: message.conversation.id.clone(),
            thread_ts: message.conversation.thread.clone(),
        };
        let channel = Anchor::from_inbound(&message, anchor.agent())
            .unwrap()
            .as_space()
            .unwrap();
        store.subscribe(channel, target, now).unwrap();
    }
    let target = PullRequestLinkContext::parse("https://github.com/acme/widget/pull/65")
        .unwrap()
        .targets
        .remove(0);
    assert_eq!(
        store.subscribe(anchor.clone(), target, now),
        Err(dekopon_agent::wake::WakeRefusal::SubscriptionsFull { maximum: 64 })
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("wakes/wakes.jsonl"))
            .unwrap()
            .lines()
            .count(),
        64
    );
}

#[test]
fn a_subscription_expires_at_its_fourteen_day_deadline_without_firing() {
    let directory = temporary();
    let store = wakes::wake_store(directory.path());
    let anchor = subscription_anchor();
    let created = std::time::SystemTime::now();
    let target = PullRequestLinkContext::parse("https://github.com/acme/widget/pull/17")
        .unwrap()
        .targets
        .remove(0);
    let summary = store
        .subscribe(anchor.clone(), target.clone(), created)
        .unwrap();
    let expiry = Duration::from_secs(14 * 24 * 60 * 60);
    assert_eq!(summary.due_in, expiry);
    let repeated = store
        .subscribe(anchor.clone(), target, created + Duration::from_secs(60))
        .unwrap();
    assert_eq!(repeated.id, summary.id);
    assert_eq!(repeated.due_in, expiry - Duration::from_secs(60));
    let due = store.take_due(created + expiry).unwrap();
    assert!(due.fired.is_empty());
    assert!(due.ticks.is_empty());
    assert!(
        store
            .list_subscriptions(&anchor, created + expiry)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn only_a_confirmed_review_of_a_link_target_creates_a_space_subscription() {
    use dekopon_shell::CapabilityCallResult;
    let directory = temporary();
    let store = wakes::wake_store(directory.path());
    let observer = crate::wake::ReviewSubscriptions::Subscribe {
        anchor: Box::new(subscription_anchor()),
        store: Arc::clone(&store),
        links: PullRequestLinkContext::parse("https://github.com/acme/widget/pull/17").unwrap(),
    };
    let input = json!({"owner":"acme","repo":"widget","number":17});
    observer.reviewed(
        "gh.pull-request.comment",
        &input,
        CapabilityCallResult::Denied {
            reason: "policy".into(),
        },
    );
    observer.reviewed(
        "gh.pull-request.comment",
        &json!({"owner":"acme","repo":"other","number":17}),
        CapabilityCallResult::Succeeded,
    );
    assert!(
        store
            .list_subscriptions(&subscription_anchor(), std::time::SystemTime::now())
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        observer.reviewed(
            "gh.pull-request.comment",
            &input,
            CapabilityCallResult::Succeeded
        ),
        CapabilityCallResult::Succeeded
    ));
    let rows = fs::read_to_string(directory.path().join("wakes/wakes.jsonl")).unwrap();
    let row: Value = serde_json::from_str(rows.trim()).unwrap();
    assert_eq!(row["subject"], subscription_anchor().subject().canonical());
    assert_eq!(
        row["subscription"],
        json!({"repository":"acme/widget","number":17})
    );
    assert_eq!(
        store
            .list_subscriptions(&subscription_anchor(), std::time::SystemTime::now())
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_wake_decline_makes_no_slack_http_calls() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::chat_capabilities(
            vec![capability("cli-probe.upper")],
            Vec::new(),
            BTreeMap::new(),
            None,
            Some("channel-space".parse().unwrap()),
        )],
    )
    .await;
    let models = ModelScript::new([decline_reply()]);
    let running = runner(broker, Arc::clone(&models), 4);
    let http = spawn_http_mock(slack_handler(Vec::new()));
    let transport = slack_with(
        &http.base,
        SlackExperience::Classic,
        LivenessConfig::default(),
    );
    let message = subscription_wake();
    let mut bound = route(model_config());
    bound.transport = message.transport.clone();
    bound.wakes = true;
    run_session(running, bound, message, transport.driver()).await;
    assert_eq!(models.requests(), 1);
    assert!(http.calls().is_empty());
}

pub(super) fn subscription_wake() -> InboundMessage {
    let mut message = owned_slack_message("Refresh this PR and decline if unchanged.", false);
    message.subject = dekopon_core::AttestedSubject::one(subscription_anchor().subject().clone())
        .expect("one-form subject");
    message.message_id = MessageId::Wake {
        id: dekopon_agent::wake::WakeId(1),
        notice: "review".into(),
        origin: crate::wake::WakeOrigin::Subscription(
            PullRequestLinkContext::parse("https://github.com/acme/widget/pull/17")
                .unwrap()
                .targets
                .remove(0),
        ),
    };
    message.thread_continuation = None;
    message
}

#[tokio::test]
async fn a_revoked_space_subscription_wake_makes_no_slack_http_calls() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let refusal = dekopon_broker_protocol::SessionRefusal::AgentDenied {
        principal: "channel-space".parse().unwrap(),
    };
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::session_refused(&refusal)],
    )
    .await;
    let models = ModelScript::new([]);
    let running = runner(broker, Arc::clone(&models), 4);
    let http = spawn_http_mock(slack_handler(Vec::new()));
    let transport = slack_with(
        &http.base,
        SlackExperience::Classic,
        LivenessConfig::default(),
    );
    let mut bound = route(model_config());
    bound.transport = "scientist-slack".into();
    bound.wakes = true;
    run_session(running, bound, subscription_wake(), transport.driver()).await;
    assert_eq!(models.requests(), 0);
    assert!(http.calls().is_empty());
    let request = observed.try_recv().expect("fresh space authorization");
    let BrokerRequest::Capabilities {
        attestation: Some(claim),
    } = request.request
    else {
        panic!("space leg")
    };
    assert_eq!(claim.subjects.name(), subscription_anchor().subject());
    let scope = claim.scope.unwrap();
    assert_eq!(scope.trigger, dekopon_broker_protocol::Trigger::Wake);
    assert!(scope.participant.is_none());
    let rejections = capture.events_where(
        "dekopon_gatewayd",
        &[
            ("event", "gateway_session_rejected"),
            ("reason", "agent-denied"),
        ],
    );
    assert_eq!(rejections.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn subscription_authorization_and_connection_failures_remain_audited_without_slack_http_calls()
 {
    use dekopon_broker_protocol::{ERROR_UNAUTHENTICATED, SessionRefusal};
    for (response, event, reason) in [
        (
            Some(ResponseEnvelope::session_refused(
                &SessionRefusal::UnmappedSubject,
            )),
            "gateway_session_rejected",
            Some("unmapped-subject"),
        ),
        (
            Some(ResponseEnvelope::error(ERROR_UNAUTHENTICATED, "refused")),
            "gateway_session_rejected",
            Some("attestation-refused"),
        ),
        (
            Some(ResponseEnvelope::chat_capabilities(
                Vec::new(),
                Vec::new(),
                BTreeMap::new(),
                None,
                Some("channel-space".parse().unwrap()),
            )),
            "gateway_session_rejected",
            Some("unauthorized"),
        ),
        (None, "gateway_session_failed", None),
    ] {
        let (capture, _guard) = capture_spans();
        let directory = temporary();
        let broker = if let Some(response) = response {
            stub_broker(directory.path(), vec![response]).await.0
        } else {
            ResolvedBroker {
                socket_path: directory.path().join("absent.sock"),
                server_uid: crate::current_uid(),
                frame: FrameLimits::default(),
            }
        };
        let models = ModelScript::new([]);
        let running = runner(broker, Arc::clone(&models), 4);
        let http = spawn_http_mock(slack_handler(Vec::new()));
        let transport = slack_with(
            &http.base,
            SlackExperience::Classic,
            LivenessConfig::default(),
        );
        let mut bound = route(model_config());
        bound.transport = "scientist-slack".into();
        bound.wakes = true;
        run_session(running, bound, subscription_wake(), transport.driver()).await;
        assert_eq!(models.requests(), 0);
        assert!(http.calls().is_empty());
        let records = capture.events_where("dekopon_gatewayd", &[("event", event)]);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].field("reason"), reason);
    }
}

#[tokio::test]
async fn busy_and_sealed_subscription_wakes_remain_audited_without_slack_http_calls() {
    for sealed in [false, true] {
        let (capture, _guard) = capture_spans();
        let directory = temporary();
        let (broker, _observed) = stub_broker(directory.path(), Vec::new()).await;
        let models = ModelScript::new([]);
        let running = runner(broker, Arc::clone(&models), 1);
        let http = spawn_http_mock(slack_handler(Vec::new()));
        let transport = slack_with(
            &http.base,
            SlackExperience::Classic,
            LivenessConfig::default(),
        );
        let mut bound = route(model_config());
        bound.transport = "scientist-slack".into();
        bound.wakes = true;
        let message = subscription_wake();
        let held = if sealed {
            let key = crate::conversation::ConversationKey::of(
                &bound.agent,
                &bound.transport,
                &message.conversation,
                Some(message.subject.name()),
            );
            assert!(crate::session::seal_conversation(&running, &bound, &message, &key).await);
            None
        } else {
            Some(
                running
                    .gate
                    .admit_probe(("dev".into(), "another-conversation".into()))
                    .unwrap(),
            )
        };
        run_session(running, bound, message, transport.driver()).await;
        drop(held);
        assert_eq!(models.requests(), 0);
        assert!(http.calls().is_empty());
        assert_eq!(
            capture
                .events_where(
                    "dekopon_gatewayd",
                    &[
                        ("event", "gateway_session_rejected"),
                        ("reason", if sealed { "sealed" } else { "busy" })
                    ]
                )
                .len(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_review_that_refreshed_no_review_cannot_decline_after_capability_work() {
    for max_steps in [2, 3] {
        let directory = temporary();
        let (broker, mut observed) = stub_broker(
            directory.path(),
            vec![
                probe_listing(),
                upper_proposal("done"),
                ResponseEnvelope::invocation(record_success(), Vec::new(), Vec::new(), Vec::new()),
            ],
        )
        .await;
        let models = ModelScript::new([
            script_call("probe upper --text done"),
            decline_reply(),
            answer("I ran a capability but refreshed no GitHub review."),
        ]);
        let running = runner(broker, Arc::clone(&models), 4);
        let http = spawn_http_mock(slack_handler(Vec::new()));
        let transport = slack_with(
            &http.base,
            SlackExperience::Classic,
            LivenessConfig::default(),
        );
        let mut bound = route(model_config());
        bound.transport = "scientist-slack".into();
        bound.wakes = true;
        bound.limits.max_steps = max_steps;
        run_session(running, bound, subscription_wake(), transport.driver()).await;
        assert_eq!(models.requests(), max_steps as usize);
        let mut invocations = 0;
        while let Ok(request) = observed.try_recv() {
            if let BrokerRequest::Run { word, .. } = request.request {
                assert_eq!(word, "probe");
                invocations += 1;
            }
        }
        assert_eq!(invocations, 1);
        let posts = http
            .calls()
            .into_iter()
            .filter(|(path, _)| path == "/api/chat.postMessage")
            .collect::<Vec<_>>();
        assert_eq!(posts.len(), 1);
        let body: Value = serde_json::from_str(&posts[0].1).unwrap();
        assert_eq!(
            body["text"],
            if max_steps == 2 {
                crate::session::UNREPORTED_WORK_REPLY
            } else {
                "I ran a capability but refreshed no GitHub review."
            }
        );
    }
}

#[test]
fn only_a_successful_review_of_the_subscribed_target_confirms_a_refresh() {
    use dekopon_agent::prompt::ReviewProgress;
    use dekopon_shell::CapabilityCallResult;
    let review_target = PullRequestLinkContext::parse("https://github.com/acme/widget/pull/17")
        .unwrap()
        .targets
        .remove(0);
    let target = json!({"owner":"acme","repo":"widget","number":17});
    for (capability, input, outcome) in [
        (
            "other.write",
            target.clone(),
            CapabilityCallResult::Succeeded,
        ),
        (
            "gh.pull-request.comment",
            target.clone(),
            CapabilityCallResult::Failed {
                error: "failed".into(),
                detail: None,
            },
        ),
        (
            "gh.pull-request.comment",
            target.clone(),
            CapabilityCallResult::Denied {
                reason: "policy".into(),
            },
        ),
        (
            "gh.pull-request.comment",
            target.clone(),
            CapabilityCallResult::Exited {
                status: std::num::NonZeroU8::new(1).unwrap(),
                stderr: "failed".into(),
            },
        ),
        (
            "gh.pull-request.comment",
            target.clone(),
            CapabilityCallResult::NotFound,
        ),
        (
            "gh.pull-request.comment",
            json!({"owner":"acme","repo":"widget","number":18}),
            CapabilityCallResult::Succeeded,
        ),
        (
            "gh.pull-request.comment",
            json!({"owner":"acme","repo":"other","number":17}),
            CapabilityCallResult::Succeeded,
        ),
        (
            "gh.pull-request.comment",
            json!({"owner":"other","repo":"widget","number":17}),
            CapabilityCallResult::Succeeded,
        ),
    ] {
        let progress = Arc::new(ReviewProgress::default());
        let observer = crate::wake::ReviewSubscriptions::Refresh {
            target: review_target.clone(),
            progress: Arc::clone(&progress),
        };
        assert_eq!(
            observer.reviewed(capability, &input, outcome.clone()),
            outcome
        );
        assert!(!progress.permits_decline(1));
        observer.reviewed(
            "gh.pull-request.comment",
            &target,
            CapabilityCallResult::Succeeded,
        );
        assert!(!progress.permits_decline(1));
        assert!(!progress.permits_decline(2));
    }
    for outcome in [
        CapabilityCallResult::Succeeded,
        CapabilityCallResult::SucceededWithStderr("notice".into()),
    ] {
        let progress = Arc::new(ReviewProgress::default());
        let observer = crate::wake::ReviewSubscriptions::Refresh {
            target: review_target.clone(),
            progress: Arc::clone(&progress),
        };
        assert_eq!(
            observer.reviewed("gh.pull-request.comment", &target, outcome.clone()),
            outcome
        );
        assert!(progress.permits_decline(1));
        observer.reviewed(
            "gh.pull-request.comment",
            &target,
            CapabilityCallResult::Succeeded,
        );
        assert!(progress.permits_decline(2));
    }
}

#[test]
fn a_subscription_close_is_recorded_separately_from_deadline_expiry() {
    let (capture, _guard) = capture_spans();
    let directory = temporary();
    let store = wakes::wake_store(directory.path());
    let now = std::time::SystemTime::now();
    for (number, created) in [
        (17, now),
        (18, now - Duration::from_secs(14 * 24 * 60 * 60)),
    ] {
        let target =
            PullRequestLinkContext::parse(&format!("https://github.com/acme/widget/pull/{number}"))
                .unwrap()
                .targets
                .remove(0);
        store
            .subscribe(subscription_anchor(), target, created)
            .unwrap();
    }
    let event = serde_json::from_value(json!({
        "delivery":"closed", "event":"pull_request", "action":"closed", "number":17,
        "repository":{"id":10,"fullName":"acme/widget"}, "actor":{"id":1},
        "kind":"none", "authorSet":{"actorLogin":"alice"},
    }))
    .unwrap();
    let delivery = store
        .deliver_subscriptions(subscription_anchor().agent(), &event, now)
        .unwrap();
    assert!(delivery.matched);
    assert!(delivery.fired.is_empty());
    assert!(
        store
            .list_subscriptions(&subscription_anchor(), now)
            .unwrap()
            .is_empty()
    );
    for (operation, number) in [("closed", "17"), ("expired", "18")] {
        let records = capture.events_where(
            "dekopon_gatewayd::wake::store",
            &[("subscription.operation", operation)],
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].field("github.number"), Some(number));
    }
}
