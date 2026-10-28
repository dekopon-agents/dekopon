use super::*;
use crate::{collection::Dispositions, pull_request_links::PullRequestLinkContext, session::Admit};

const PR: &str = "https://github.com/acme/widget/pull/17";

#[test]
fn pull_request_targets_are_parsed_deduplicated_and_rendered_as_structured_data() {
    let links = PullRequestLinkContext::parse(&format!(
        "<{PR}|review> HTTPS://GITHUB.COM/ACME/WIDGET/pull/17/files?x=1#diff \
         https://github.com/other/repo.name/pull/18/checks/"
    ))
    .unwrap();
    assert_eq!(links.targets.len(), 2);
    assert_eq!(links.targets[0].owner, "acme");
    assert_eq!(links.targets[0].repo, "widget");
    assert_eq!(
        links.targets[0].number,
        dekopon_core::github::GithubNumber::new(17).unwrap()
    );
    let prompt = links.prompt();
    let (_, targets) = prompt.split_once("Targets: ").unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(targets).unwrap(),
        json!([
            {"owner": "acme", "repo": "widget", "number": 17},
            {"owner": "other", "repo": "repo.name", "number": 18}
        ])
    );
    for url in [
        "https://github.com/acme/widget",
        "https://github.com/acme/widget/issues/17",
        "https://github.com.evil/acme/widget/pull/17",
        "https://github.com@evil/acme/widget/pull/17",
        "http://github.com/acme/widget/pull/17",
        "https://github.com:444/acme/widget/pull/17",
        "https://user@github.com/acme/widget/pull/17",
        "https://github.com/acme/widget/pull/0",
        "https://github.com/acme/widget/pull/01",
        "https://github.com/acme/widget/pull/9223372036854775808",
        "https://github.com/acme/widget/pull/17/unknown",
        "https://github.com/acme/widget/pull/17x",
        "https://github.com/acme%2felse/widget/pull/17",
    ] {
        assert!(PullRequestLinkContext::parse(url).is_none(), "{url}");
    }
}

#[test]
fn a_non_pr_slack_hyperlink_does_not_start_a_review_from_its_display_label() {
    assert!(
        PullRequestLinkContext::parse(
            "<https://github.com/acme/widget/issues/17|https://github.com/other/repo/pull/18>"
        )
        .is_none()
    );
}

#[test]
fn a_pr_slack_hyperlink_reviews_only_its_url_and_bare_links_outside_it() {
    let links = PullRequestLinkContext::parse(&format!(
        "{PR} <{PR}|https://github.com/other/repo/pull/18> \
         https://github.com/third/project/pull/19"
    ))
    .unwrap();
    assert_eq!(links.targets.len(), 2);
    assert_eq!(links.targets[0].owner, "acme");
    assert_eq!(links.targets[0].repo, "widget");
    assert_eq!(
        links.targets[0].number,
        dekopon_core::github::GithubNumber::new(17).unwrap()
    );
    assert_eq!(links.targets[1].owner, "third");
    assert_eq!(links.targets[1].repo, "project");
    assert_eq!(
        links.targets[1].number,
        dekopon_core::github::GithubNumber::new(19).unwrap()
    );
}

#[test]
fn a_truncated_pr_link_is_never_reinterpreted_as_a_shorter_number() {
    let url = "https://github.com/acme/widget/pull/123";
    let text = format!(
        "{}{url}",
        " ".repeat(MAX_INBOUND_TEXT_BYTES - url.len() + 1)
    );
    assert!(PullRequestLinkContext::parse(&text).is_none());
}

#[test]
fn truncation_keeps_complete_slack_hyperlinks_and_drops_incomplete_ones() {
    let complete = format!("<{PR}|review>{}", "x".repeat(MAX_INBOUND_TEXT_BYTES));
    let links = PullRequestLinkContext::parse(&complete).unwrap();
    assert_eq!(links.targets.len(), 1);
    assert_eq!(
        links.targets[0].number,
        dekopon_core::github::GithubNumber::new(17).unwrap()
    );
    let incomplete = format!(
        "<https://github.com/acme/widget/issues/17|{PR} {}>",
        "x".repeat(MAX_INBOUND_TEXT_BYTES)
    );
    assert!(PullRequestLinkContext::parse(&incomplete).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_keyed_unaddressed_human_channel_pr_link_is_admitted() {
    for keyed in [false, true] {
        for experience in [SlackExperience::Classic, SlackExperience::Agent] {
            let fixture = SlackFollowFixture::with_pull_request_links(keyed).await;
            let mut bot = channel_message("u9xyz", "1700000000.000002", None, PR);
            bot["bot_id"] = json!("BOTHER");
            let mut edit = channel_message("u9xyz", "1700000000.000003", None, PR);
            edit["subtype"] = json!("message_changed");
            let mut dm = channel_message("u9xyz", "1700000000.000006", None, PR);
            dm["channel_type"] = json!("im");
            dm["channel"] = json!("d0123abc");
            let mut frames = vec![
                bot,
                edit,
                channel_message("u9xyz", "1700000000.000004", Some("1700000000.000001"), PR),
                channel_message(
                    "u9xyz",
                    "1700000000.000005",
                    None,
                    "https://github.com/acme/widget/issues/17",
                ),
                channel_message(
                    "u9xyz",
                    "1700000000.000009",
                    None,
                    "<https://github.com/acme/widget/issues/17|https://github.com/other/repo/pull/18>",
                ),
                dm,
            ];
            if !keyed {
                frames.push(channel_message("u9xyz", "1700000000.000007", None, PR));
            }
            frames.push(channel_message(
                "u9xyz",
                "1700000000.000008",
                None,
                &format!("<@{BOT_USER}> {PR}"),
            ));
            let socket = spawn_socket_mock(
                frames
                    .into_iter()
                    .enumerate()
                    .map(|(i, event)| events_envelope(&i.to_string(), event))
                    .collect(),
            );
            let http = spawn_http_mock(slack_handler(vec![socket.url.clone()]));
            let mut transport = slack_with(&http.base, experience, LivenessConfig::default())
                .with_generation(fixture.generation.clone());
            transport.connect().await.unwrap();
            let direct = next_message(&mut transport).await;
            assert_eq!(
                direct.message_id,
                MessageId::Native("1700000000.000006".into())
            );
            assert_eq!(direct.conversation.kind, ConversationKind::DirectMessage);
            assert!(direct.pull_request_links.is_none());
            let mentioned = next_message(&mut transport).await;
            assert_eq!(
                mentioned.message_id,
                MessageId::Native("1700000000.000008".into())
            );
            assert_eq!(mentioned.addressed, Some(true));
            assert!(mentioned.pull_request_links.is_none());
        }
    }
}

#[tokio::test]
async fn a_pr_link_starts_one_poster_authorized_threaded_turn_and_opens_the_follow_ticket() {
    for mapped in [false, true] {
        for experience in [SlackExperience::Classic, SlackExperience::Agent] {
            let (capture, _guard) = capture_spans();
            let fixture = SlackFollowFixture::with_pull_request_links(true).await;
            let event = channel_message(
                "u9xyz",
                "1700000000.000001",
                Some("1700000000.000001"),
                &format!("<{PR}|https://github.com/other/repo/pull/18> {PR}"),
            );
            let socket = spawn_socket_mock(vec![
                events_envelope("start", event.clone()),
                events_envelope("duplicate", event),
            ]);
            let fallback = slack_handler(vec![socket.url.clone()]);
            let http = spawn_http_mock(move |path, body| {
                if path == "/api/chat.postMessage" {
                    let posted: Value = serde_json::from_str(body).unwrap();
                    return json!({"ok": true, "channel": posted["channel"], "ts": "1700000000.000100"});
                }
                fallback(path, body)
            });
            let mut transport = slack_with(&http.base, experience, LivenessConfig::default())
                .with_generation(fixture.generation.clone());
            transport.connect().await.unwrap();
            let inbound = next_message(&mut transport).await;
            assert_eq!(inbound.addressed, Some(false));
            assert_eq!(
                inbound.subject,
                ExternalSubject::slack(TEAM, "u9xyz").unwrap()
            );
            assert_eq!(
                inbound.pull_request_links.as_ref().unwrap().targets.len(),
                1
            );
            assert!(
                matches!(&inbound.reply, ReplyTarget::Slack { thread_ts: Some(root), .. } if root == "1700000000.000001")
            );
            let mut broker = dekopon_test_support::BrokerFixture::answering(fixture.directory.path(), [
                ResponseEnvelope::chat_capabilities(vec![capability("dekopon-provider-follow.stay")], vec!["follow".into()], BTreeMap::new(), None, Some("poster".parse().unwrap()))
                    .with_space(mapped.then(|| vec!["dekopon-provider-follow.stay".parse().unwrap()])),
                ResponseEnvelope::command_run(serde_json::from_value(json!({"outcome":"proposed", "capability":"dekopon-provider-follow.stay", "input":{}})).unwrap()),
                ResponseEnvelope::invocation(record_success(), vec![], vec![], vec![]),
            ]);
            let models = ModelScript::new([
                script_call("follow stay"),
                answer("Reviewed: https://github.com/acme/widget/pull/17#pullrequestreview-1"),
            ]);
            let mut running = Arc::into_inner(runner(
                ResolvedBroker {
                    socket_path: broker.socket().to_path_buf(),
                    server_uid: crate::current_uid(),
                    frame: FrameLimits::default(),
                },
                Arc::clone(&models),
                4,
            ))
            .unwrap();
            running.follow = Some(Arc::clone(&fixture.table));
            let driver = transport.driver();
            let gateway = crate::gateway::Gateway::new(
                Arc::new(running),
                Arc::clone(&fixture.routes),
                Arc::default(),
                Arc::new(BTreeMap::from([("scientist-slack".into(), driver)])),
                Arc::default(),
            );
            let mut sessions = tokio::task::JoinSet::new();
            gateway.dispatch(
                &mut sessions,
                &mut crate::collection::Collector::new(&[], 4),
                &mut crate::follow::Debounce::default(),
                inbound.clone(),
            );
            assert_eq!(sessions.len(), 1);
            tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(result) = sessions.join_next().await {
                    result.unwrap();
                }
            })
            .await
            .unwrap();
            assert_eq!(models.requests(), 2);
            assert!(models.prompt(0).iter().any(|(role, text)| role == "system"
                && text.contains("pull-request-link trigger")
                && text.contains(r#"{"owner":"acme","repo":"widget","number":17}"#)));
            assert!(matches!(
                fixture.table.ticket(&fixture.key, SystemTime::now()),
                crate::follow::FollowTicket::Active { .. }
            ));
            let posts: Vec<Value> = {
                let calls = http.calls.lock();
                calls
                    .iter()
                    .filter(|(path, _)| path == "/api/chat.postMessage")
                    .map(|(_, body)| serde_json::from_str(body).unwrap())
                    .collect()
            };
            assert_eq!(posts.len(), 1);
            assert_eq!(posts[0]["thread_ts"], "1700000000.000001");
            assert_eq!(
                capture
                    .events_where(
                        "dekopon_gatewayd::audit",
                        &[
                            ("audit.event", "gateway.admission"),
                            ("admission.trigger", "pull-request-link"),
                            ("outcome", "started")
                        ]
                    )
                    .len(),
                1
            );
            let request = broker.request().await;
            assert!(
                matches!(request.request, BrokerRequest::Capabilities { attestation: Some(Attestation { subject, scope: Some(ref scope), .. }) } if subject == inbound.subject && scope.trigger == dekopon_broker_protocol::Trigger::Message)
            );
            let runs = broker
                .recorded()
                .into_iter()
                .filter(|request| matches!(request.request, BrokerRequest::Run { .. }))
                .collect::<Vec<_>>();
            assert_eq!(runs.len(), 1);
            let BrokerRequest::Run {
                word,
                argv,
                attestation: Some(claim),
                ..
            } = &runs[0].request
            else {
                panic!("follow command expected")
            };
            assert_eq!(word, "follow");
            assert_eq!(argv, &[dekopon_broker_protocol::Arg::Text("stay".into())]);
            assert_eq!(claim.subject, inbound.subject);
            assert_eq!(
                claim.scope.as_ref().unwrap().participant.as_deref(),
                mapped.then_some("p1")
            );
        }
    }
}

#[test]
fn other_speakers_stay_queued_before_and_after_pr_link_mapping_resolves() {
    for resolved_first in [false, true] {
        let gate = SessionGate::new(1);
        let route = persistent_route(model_config(), shared_window());
        let mut first = owned_slack_message(PR, false);
        first.pull_request_links = PullRequestLinkContext::parse(PR);
        let key = (first.transport.clone(), first.conversation.key());
        let Admit::Admitted(admission, _, _) = gate.admit(&route, first, Dispositions(vec![]))
        else {
            panic!("PR link admitted")
        };
        if resolved_first {
            gate.resolve_mapping(&key, &admission.cancellation(), true);
        }
        let mut other = owned_slack_message("Bob asks", false);
        other.subject = ExternalSubject::slack(TEAM, "u8other").unwrap();
        assert!(matches!(
            gate.admit(&route, other.clone(), Dispositions(vec![])),
            Admit::Queued
        ));
        if !resolved_first {
            gate.resolve_mapping(&key, &admission.cancellation(), true);
        }
        assert!(gate.take_steers(&key).is_empty());
        let (_, next) = admission.next_or_release().unwrap();
        assert_eq!(next.message.subject, other.subject);
        assert!(next.message.pull_request_links.is_none());
    }
}
