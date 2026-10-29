use super::subscriptions::{meta, response, stored, wait_event};
use super::*;
use dekopon_gatewayd::fauxlack::Fauxlack;
use dekopon_test_support::LoopbackServer;
use std::time::{SystemTime, UNIX_EPOCH};

const WAIT: Duration = Duration::from_secs(30);
const DAY: u64 = 86_400;

pub(super) fn configure_broker(config: &mut Value, policies: &mut String, api: Option<&str>) {
    config["principals"]["random-space"] = json!({"subjects":["fauxlack.acme.crandom"]});
    policies.push_str(&format!(r#"
@id("digest-other-space-prompt")
permit(principal == Dekopon::Principal::"random-space", action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"{AGENT}") when {{ context.via == "{GATEWAY_PRINCIPAL}" }};
@id("digest-other-space-read")
permit(principal == Dekopon::Principal::"random-space", action == Dekopon::Action::"cli-probe.upper", resource == Dekopon::Provider::"cli-probe") when {{ context.via == "{GATEWAY_PRINCIPAL}" && context.agent == "{AGENT}" }};
"#));
    if let Some(api) = api {
        config["providers"]
            .as_array_mut()
            .unwrap()
            .push(json!(provider("gh")));
        config["providerSettings"]["gh"] = json!({"baseUrl":format!("{api}/api/v3")});
        let authority = reqwest::Url::parse(api).unwrap().authority().to_owned();
        config["capabilities"]["gh"] = json!({
            "constraints":{"timeoutMs":30000,"http":{"allowedHosts":[authority],"allowedMethods":["GET"],"maxRequests":1,"maxRequestBytes":16384,"maxResponseBytes":65536,"allowPlaintextLoopback":true}},
            "capabilities":{"gh.issue.list":{}}
        });
        for principal in ["alice", "team"] {
            policies.push_str(&format!(r#"
@id("digest-{principal}-read")
permit(principal == Dekopon::Principal::"{principal}", action == Dekopon::Action::"gh.issue.list", resource == Dekopon::Provider::"gh") when {{ context.via == "{GATEWAY_PRINCIPAL}" && context.agent == "{AGENT}" }};
"#));
        }
    }
}

async fn boot_digests(
    chat: &Fauxlack,
    directory: tempfile::TempDir,
    responses: Vec<Value>,
    rows: Vec<Value>,
    api: Option<String>,
) -> Fixture {
    boot_with(
        directory,
        responses,
        None,
        "plain",
        false,
        &Timing {
            fauxlack: Some(chat.clone()),
            fauxlack_servers: vec!["acme".into()],
            wakes: Some(rows),
            digest_api: api,
            ..Timing::default()
        },
    )
    .await
}

fn request(action: &str, script: &str) -> Value {
    match action {
        "schedule" => json!({"action":action,"note":"Waiting on octocat","afterSeconds":600}),
        "watch" => {
            json!({"action":action,"note":"Waiting on octocat","script":script,"everySeconds":600,"forSeconds":DAY*2})
        }
        "digest" => {
            json!({"action":action,"note":"Waiting on octocat","script":script,"everySeconds":DAY,"forSeconds":DAY*2})
        }
        _ => panic!("fixture action"),
    }
}

fn due(row: &mut Value) {
    row["nextAtMs"] = json!(
        u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        )
        .unwrap()
            - 1
    );
    if let Some(watch) = row.get_mut("watch") {
        watch["prev"] = json!("prior snapshot");
    }
}

fn assert_space_read(audit: &Audit, capability: &str, trigger: &str, root: &str) {
    let decisions = audit.capture.events_where(
        "dekopon_broker",
        &[
            ("audit.event", "broker.decision"),
            ("capability.id", capability),
            ("decision.point", "invocation"),
            ("turn.started_by", trigger),
        ],
    );
    assert!(!decisions.is_empty(), "space {trigger} read observed");
    for decision in decisions {
        assert_eq!(decision.field("principal"), Some("team"));
        assert_eq!(decision.field("principal.kind"), Some("space"));
        assert_eq!(decision.field("space.id"), Some("team"));
        assert_eq!(decision.field("participant.handle"), None);
        assert_eq!(decision.field("asker.allowed"), None);
        assert_eq!(decision.field("conversation.thread"), Some(root));
        assert_eq!(decision.field("decision.allowed"), Some("true"));
    }
}

async fn wait_digest(audit: &Audit, operation: &str, previous: usize) {
    tokio::time::timeout(WAIT, async {
        loop {
            if audit
                .capture
                .events_where(
                    "dekopon_gatewayd",
                    &[
                        ("audit.event", "gateway.admission"),
                        ("digest.operation", operation),
                    ],
                )
                .len()
                > previous
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("digest lifecycle observed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_channel_digest_stores_space_ownership_and_fires_in_its_thread_with_no_person_grants() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let issue = json!([{"number":17,"title":"Unowned issue","state":"open","assignees":[],"created_at":"2026-10-01T00:00:00Z","updated_at":"2026-10-01T00:00:00Z"}]);
    let api = LoopbackServer::sequence([
        response(issue.clone()),
        response(issue.clone()),
        response(issue),
    ]);
    let script = "gh issue list -R acme/widget --json number,title";
    let fixture = boot_digests(
        &chat,
        temporary(),
        vec![
            meta("create", "wake", request("digest", script)),
            final_answer("Digest set."),
        ],
        vec![],
        Some(api.url()),
    )
    .await;
    let root = chat
        .server("acme")
        .user("ualice")
        .post("cteam", "<@udekopon> post a daily digest here")
        .await
        .unwrap();
    assert_eq!(
        root.thread().next_from("udekopon", WAIT).await.text,
        "Digest set."
    );
    let mut rows = stored(&fixture);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["run_as"], "space");
    assert_eq!(rows[0]["subject"], "fauxlack.acme.cteam");
    assert_eq!(rows[0]["conversation"]["thread"], root.id());
    assert_eq!(rows[0]["watch"]["everyMs"], DAY * 1000);
    assert_eq!(
        rows[0]["watch"]["untilMs"].as_u64().unwrap() - rows[0]["nextAtMs"].as_u64().unwrap(),
        DAY * 1000
    );
    let directory = fixture.shutdown().await;
    due(&mut rows[0]);
    let fixture = boot_digests(
        &chat,
        directory,
        vec![
            meta("reason", "wake_reason", json!({})),
            bash_tool_call("read", script),
            final_answer("Unowned issue #17 waits for an owner; octocat can help."),
        ],
        rows,
        Some(api.url()),
    )
    .await;
    assert_eq!(
        root.thread().next_from("udekopon", WAIT).await.text,
        "Unowned issue #17 waits for an owner; octocat can help."
    );
    assert_space_read(&audit, "gh.issue.list", "probe", root.id());
    assert_space_read(&audit, "gh.issue.list", "wake", root.id());
    let reason: Value = serde_json::from_str(&fixture.tool_result("reason")).unwrap();
    assert!(reason["scheduledBy"].is_null());
    assert_eq!(reason["trigger"], "wake");
    assert_eq!(stored(&fixture).len(), 1, "a digest survives a fire");
    assert_eq!(
        root.thread().transcript().authors(),
        ["ualice", "udekopon", "udekopon"]
    );
    fixture.shutdown().await;
    let calls = api.recorded();
    assert_eq!(calls.len(), 3);
    for call in calls {
        let call = String::from_utf8(call).unwrap();
        assert_eq!(
            call.lines().next(),
            Some("GET /api/v3/repos/acme/widget/issues?state=open&page=1&per_page=100 HTTP/1.1")
        );
        assert!(
            !call
                .lines()
                .any(|line| line.to_ascii_lowercase().starts_with("authorization:"))
        );
    }
    api.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn channel_schedule_and_watch_wakes_run_as_space_while_dm_wakes_remain_personal() {
    let audit = Audit::exclusive().await;
    let script = "probe upper --text waiting | jq -r .text";
    for (action, container) in [
        ("schedule", "cteam"),
        ("watch", "cteam"),
        ("schedule", "dalice"),
        ("watch", "dalice"),
    ] {
        let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
        let before = audit
            .capture
            .events_where(
                "dekopon_broker",
                &[
                    ("capability.id", "cli-probe.upper"),
                    ("turn.started_by", "wake"),
                    ("principal", "alice"),
                ],
            )
            .len();
        let mut responses = vec![meta("create", "wake", request(action, script))];
        if container == "dalice" {
            responses.push(meta("personal-list", "wake", json!({"action":"list"})));
        }
        responses.push(final_answer("Set."));
        let fixture = boot_digests(&chat, temporary(), responses, vec![], None).await;
        let root = chat
            .server("acme")
            .user("ualice")
            .post(container, "<@udekopon> remind this conversation")
            .await
            .unwrap();
        assert_eq!(root.thread().next_from("udekopon", WAIT).await.text, "Set.");
        let mut rows = stored(&fixture);
        if container == "cteam" {
            assert_eq!(rows[0]["run_as"], "space");
            assert_eq!(rows[0]["subject"], "fauxlack.acme.cteam");
        } else {
            assert!(rows[0].get("run_as").is_none());
            assert_eq!(rows[0]["subject"], "fauxlack.acme.ualice");
            assert!(fixture.tool_result("personal-list").contains("wake 1:"));
        }
        let directory = fixture.shutdown().await;
        due(&mut rows[0]);
        let fixture = boot_digests(
            &chat,
            directory,
            vec![
                bash_tool_call("read", "probe upper --text fresh | jq -r .text"),
                final_answer("Fresh."),
            ],
            rows,
            None,
        )
        .await;
        assert_eq!(
            root.thread().next_from("udekopon", WAIT).await.text,
            "Fresh."
        );
        if container == "cteam" {
            assert_space_read(&audit, "cli-probe.upper", "wake", root.id());
        } else {
            let decisions = audit.capture.events_where(
                "dekopon_broker",
                &[
                    ("capability.id", "cli-probe.upper"),
                    ("turn.started_by", "wake"),
                    ("principal", "alice"),
                ],
            );
            assert_eq!(decisions.len(), before + 1);
            assert_eq!(
                decisions.last().unwrap().field("principal.kind"),
                Some("person")
            );
            assert_eq!(decisions.last().unwrap().field("space.id"), None);
        }
        assert!(
            stored(&fixture).is_empty(),
            "ordinary watches retire on fire"
        );
        fixture.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn another_mapped_participant_manages_all_channel_wake_actions_but_other_channels_and_dms_cannot()
 {
    let audit = Audit::exclusive().await;
    let manifest = include_str!("team.yaml").replace(
        "members: [ualice, ubob]",
        "members: [ualice, ubob, udekopon]",
    );
    let chat = Fauxlack::from_manifest(&manifest).unwrap();
    let script = "probe upper --text waiting | jq -r .text";
    let mut responses = Vec::new();
    for action in ["schedule", "watch", "digest"] {
        responses.push(meta(action, "wake", request(action, script)));
    }
    responses.push(final_answer("Three set."));
    let mut cancels = meta("cancel-schedule", "wake", json!({"action":"cancel","id":1}));
    for (id, call) in [(2, "cancel-watch"), (3, "cancel-digest")] {
        let next = meta(call, "wake", json!({"action":"cancel","id":id}));
        cancels["choices"][0]["message"]["tool_calls"]
            .as_array_mut()
            .unwrap()
            .extend(
                next["choices"][0]["message"]["tool_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned(),
            );
    }
    responses.extend([
        meta("outside-list", "wake", json!({"action":"list"})),
        meta("outside-cancel", "wake", json!({"action":"cancel","id":1})),
        final_answer("Outside."),
        meta("dm-list", "wake", json!({"action":"list"})),
        meta("dm-cancel", "wake", json!({"action":"cancel","id":2})),
        final_answer("Private."),
        meta("bob-list", "wake", json!({"action":"list"})),
        cancels,
        final_answer("Cancelled."),
    ]);
    let fixture = boot_digests(&chat, temporary(), responses, vec![], None).await;
    let server = chat.server("acme");
    let root = server
        .user("ualice")
        .post("cteam", "<@udekopon> set wakes")
        .await
        .unwrap();
    assert_eq!(
        root.thread().next_from("udekopon", WAIT).await.text,
        "Three set."
    );
    let rows = stored(&fixture);
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[1]["watch"]["everyMs"], 600_000,
        "D4 does not constrain an ordinary watch"
    );
    let outside = server
        .user("ualice")
        .post("crandom", "<@udekopon> list and cancel")
        .await
        .unwrap();
    assert_eq!(
        outside.thread().next_from("udekopon", WAIT).await.text,
        "Outside."
    );
    assert_eq!(fixture.tool_result("outside-list"), "No pending wakes.");
    assert_eq!(stored(&fixture).len(), 3);
    let dm = server
        .user("ualice")
        .post("dalice", "list and cancel")
        .await
        .unwrap();
    assert_eq!(
        dm.thread().next_from("udekopon", WAIT).await.text,
        "Private."
    );
    assert_eq!(fixture.tool_result("dm-list"), "No pending wakes.");
    assert_eq!(stored(&fixture).len(), 3);
    let bob = server
        .user("ubob")
        .post("cteam", "<@udekopon> list and cancel")
        .await
        .unwrap();
    assert_eq!(
        bob.thread().next_from("udekopon", WAIT).await.text,
        "Cancelled."
    );
    assert!(fixture.tool_result("bob-list").contains("wake 1:"));
    assert!(fixture.tool_result("bob-list").contains("wake 2:"));
    assert!(fixture.tool_result("bob-list").contains("wake 3:"));
    assert!(stored(&fixture).is_empty());
    assert_eq!(
        audit
            .capture
            .events_where(
                "dekopon_agent",
                &[
                    ("audit.event", "agent.wake.refused"),
                    ("reason", "not-found")
                ]
            )
            .len(),
        2
    );
    assert_eq!(
        audit
            .capture
            .events_where("dekopon_gatewayd", &[("digest.operation", "cancelled")])
            .len(),
        1
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_digest_in_a_dm_and_a_second_digest_in_one_space_are_refused_before_persistence() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let call = request("digest", "probe upper --text waiting | jq -r .text");
    let fixture = boot_digests(
        &chat,
        temporary(),
        vec![
            meta("dm", "wake", call.clone()),
            final_answer("Channel required."),
            meta("first", "wake", call.clone()),
            final_answer("Set."),
            meta("second", "wake", call),
            final_answer("Already set."),
        ],
        vec![],
        None,
    )
    .await;
    let server = chat.server("acme");
    let dm = server
        .user("ualice")
        .post("dalice", "post a daily digest here")
        .await
        .unwrap();
    assert_eq!(
        dm.thread().next_from("udekopon", WAIT).await.text,
        "Channel required."
    );
    assert!(stored(&fixture).is_empty());
    for (user, answer) in [("ualice", "Set."), ("ubob", "Already set.")] {
        let root = server
            .user(user)
            .post("cteam", "<@udekopon> post a daily digest here")
            .await
            .unwrap();
        assert_eq!(root.thread().next_from("udekopon", WAIT).await.text, answer);
    }
    assert_eq!(stored(&fixture).len(), 1);
    for reason in ["mapped-channel-required", "digest-exists"] {
        assert_eq!(
            audit
                .capture
                .events_where(
                    "dekopon_agent",
                    &[("audit.event", "agent.wake.refused"), ("reason", reason)]
                )
                .len(),
            1
        );
    }
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unchanged_and_empty_digest_probes_start_no_model_turn_and_post_nothing() {
    let audit = Audit::exclusive().await;
    for empty in [false, true] {
        let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
        let script = if empty {
            "probe upper --text read | jq -r '\"\"'"
        } else {
            "probe upper --text waiting | jq -r .text"
        };
        let fixture = boot_digests(
            &chat,
            temporary(),
            vec![
                meta("create", "wake", request("digest", script)),
                final_answer("Set."),
            ],
            vec![],
            None,
        )
        .await;
        let root = chat
            .server("acme")
            .user("ualice")
            .post("cteam", "<@udekopon> daily digest")
            .await
            .unwrap();
        assert_eq!(root.thread().next_from("udekopon", WAIT).await.text, "Set.");
        let mut rows = stored(&fixture);
        let directory = fixture.shutdown().await;
        rows[0]["nextAtMs"] = json!(0);
        let previous = audit
            .capture
            .events_where("dekopon_gatewayd", &[("digest.operation", "declined")])
            .len();
        let fixture = boot_digests(&chat, directory, vec![], rows, None).await;
        wait_digest(&audit, "declined", previous).await;
        assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 0);
        assert_eq!(root.thread().transcript().authors(), ["ualice", "udekopon"]);
        assert_eq!(stored(&fixture).len(), 1);
        fixture.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_digest_can_decline_after_real_read_work_without_any_chat_post() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let script = "probe upper --text waiting | jq -r .text";
    let fixture = boot_digests(
        &chat,
        temporary(),
        vec![
            meta("create", "wake", request("digest", script)),
            final_answer("Set."),
        ],
        vec![],
        None,
    )
    .await;
    let root = chat
        .server("acme")
        .user("ualice")
        .post("cteam", "<@udekopon> daily digest")
        .await
        .unwrap();
    assert_eq!(root.thread().next_from("udekopon", WAIT).await.text, "Set.");
    let mut rows = stored(&fixture);
    let directory = fixture.shutdown().await;
    due(&mut rows[0]);
    let fixture = boot_digests(
        &chat,
        directory,
        vec![
            bash_tool_call("read", script),
            meta("quiet", "decline_chat_reply", json!({})),
        ],
        rows,
        None,
    )
    .await;
    wait_event(&audit, "agent.reply.declined").await;
    assert_space_read(&audit, "cli-probe.upper", "wake", root.id());
    assert_eq!(root.thread().transcript().authors(), ["ualice", "udekopon"]);
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 2);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_digest_cannot_cancel_itself_and_renew_its_fixed_horizon() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let call = request("digest", "probe upper --text waiting | jq -r .text");
    let fixture = boot_digests(
        &chat,
        temporary(),
        vec![meta("create", "wake", call.clone()), final_answer("Set.")],
        vec![],
        None,
    )
    .await;
    let root = chat
        .server("acme")
        .user("ualice")
        .post("cteam", "<@udekopon> daily digest")
        .await
        .unwrap();
    assert_eq!(root.thread().next_from("udekopon", WAIT).await.text, "Set.");
    let mut rows = stored(&fixture);
    let expiry = rows[0]["watch"]["untilMs"].clone();
    let directory = fixture.shutdown().await;
    due(&mut rows[0]);
    let fixture = boot_digests(
        &chat,
        directory,
        vec![
            meta("cancel-self", "wake", json!({"action":"cancel", "id":1})),
            meta("renew-self", "wake", call),
            final_answer("The horizon remains fixed."),
        ],
        rows,
        None,
    )
    .await;
    assert_eq!(
        root.thread().next_from("udekopon", WAIT).await.text,
        "The horizon remains fixed."
    );
    let rows = stored(&fixture);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["watch"]["untilMs"], expiry);
    for reason in ["not-found", "mapped-channel-required"] {
        assert_eq!(
            audit
                .capture
                .events_where(
                    "dekopon_agent",
                    &[("audit.event", "agent.wake.refused"), ("reason", reason)]
                )
                .len(),
            1
        );
    }
    fixture.shutdown().await;
}
