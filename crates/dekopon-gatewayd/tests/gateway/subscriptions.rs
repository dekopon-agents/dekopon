use super::*;
use dekopon_gatewayd::fauxlack::Fauxlack;
use dekopon_test_support::LoopbackServer;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;
use std::{
    net::SocketAddr,
    time::{SystemTime, UNIX_EPOCH},
};

const WAIT: Duration = Duration::from_secs(30);

pub(super) struct Settings {
    address: SocketAddr,
    rows: Vec<Value>,
    api: Option<String>,
}

impl Settings {
    fn new(rows: Vec<Value>, api: Option<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        Self {
            address: listener.local_addr().unwrap(),
            rows,
            api,
        }
    }

    pub(super) fn configure_broker(&self, config: &mut Value, policies: &mut String) {
        config["identities"][0]["attestor"]["namespaces"]
            .as_array_mut()
            .unwrap()
            .push(json!("github"));
        config["principals"]["repository"] = json!({"subjects":["github.acme.widget"]});
        config["principals"]["outside"] = json!({"subjects":["github.acme.outside"]});
        policies.push_str(&format!(r#"
@id("subscription-repository-prompt")
permit(principal == Dekopon::Principal::"repository", action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"{AGENT}") when {{ context.via == "{GATEWAY_PRINCIPAL}" }};
"#));
        if let Some(api) = &self.api {
            config["providers"]
                .as_array_mut()
                .unwrap()
                .push(json!(provider("gh")));
            config["providerSettings"]["gh"] = json!({"baseUrl":format!("{api}/api/v3")});
            let authority = reqwest::Url::parse(api).unwrap().authority().to_owned();
            config["capabilities"]["gh"] = json!({
                "constraints":{"timeoutMs":30000,"http":{"allowedHosts":[authority],"allowedMethods":["GET","POST"],"maxRequests":2,"maxRequestBytes":16384,"maxResponseBytes":65536,"allowPlaintextLoopback":true}},
                "capabilities":{"gh.pull-request.comment":{}}
            });
            policies.push_str(&format!(r#"
@id("subscription-space-review")
permit(principal == Dekopon::Principal::"team", action == Dekopon::Action::"gh.pull-request.comment", resource == Dekopon::Provider::"gh") when {{ context.via == "{GATEWAY_PRINCIPAL}" && context.agent == "{AGENT}" }};
"#));
        }
    }

    pub(super) fn configure_gateway(&self, directory: &Path, config: &mut Value) {
        configure_wakes(directory, &self.rows, config);
        config["transports"].as_array_mut().unwrap().push(
            json!({"name":"github","kind":"githubWebhook","bind":self.address,"secretEnv":"PATH"}),
        );
        config["routes"].as_array_mut().unwrap().push(json!({"transport":"github","agent":AGENT,"githubEvents":["pull_request"],"conversation":{"kind":["thread"],"container":"acme","ids":["acme.widget","acme.unmapped"]},"memory":{"mode":"oneShot"}}));
    }
}

fn subscription(root: &str, repository: &str, id: u32) -> Value {
    let expires = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        + 14 * 24 * 60 * 60 * 1000;
    json!({"id":id,"transport":"acme-chat","kind":"fauxlack","subject":"fauxlack.acme.cteam","conversation":{"kind":"thread","container":"acme","id":"cteam","thread":root},"reply":{"kind":"fauxlack","server":"acme","container":"cteam","thread":root},"agent":AGENT,"note":"review this PR","nextAtMs":u64::try_from(expires).unwrap(),"subscription":{"repository":repository,"number":17}})
}

async fn boot_subscriptions(chat: &Fauxlack, responses: Vec<Value>, settings: Settings) -> Fixture {
    let address = settings.address;
    let fixture = boot_with(
        temporary(),
        responses,
        None,
        "plain",
        true,
        &Timing {
            fauxlack: Some(chat.clone()),
            fauxlack_servers: vec!["acme".into()],
            subscriptions: Some(settings),
            ..Timing::default()
        },
    )
    .await;
    tokio::time::timeout(WAIT, async {
        loop {
            match tokio::net::TcpStream::connect(address).await {
                Ok(socket) => {
                    drop(socket);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(error) => panic!("webhook readiness: {error}"),
            }
        }
    })
    .await
    .expect("subscription webhook listener starts");
    fixture
}

pub(super) fn stored(fixture: &Fixture) -> Vec<Value> {
    fs::read_to_string(fixture.directory.path().join("wakes.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

pub(super) fn meta(id: &str, name: &str, arguments: Value) -> Value {
    json!({"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]}}]})
}

async fn deliver(address: SocketAddr, repo: &str, action: &str, delivery: &str) {
    let body = json!({
        "action":action,"number":17,
        "repository":{"id":10,"owner":{"login":"acme"},"name":repo},
        "sender":{"id":1,"login":"alice","type":"User"},
        "pull_request":{"number":17,"draft":false,"state":if action=="closed" {"closed"} else {"open"},"merged":action=="closed","user":{"id":1},"author_association":"MEMBER","labels":[],"base":{"ref":"main","sha":"b".repeat(40),"repo":{"id":10}},"head":{"ref":"feature","sha":"a".repeat(40),"repo":{"id":10,"full_name":format!("acme/{repo}")}}}
    }).to_string();
    let mut mac =
        Hmac::<Sha256>::new_from_slice(std::env::var("PATH").unwrap().as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let signature = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let response = reqwest::Client::new()
        .post(format!("http://{address}/webhooks/github"))
        .header("x-github-event", "pull_request")
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", format!("sha256={signature}"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

pub(super) fn response(body: Value) -> Vec<u8> {
    let body = body.to_string();
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).into_bytes()
}

fn review_api() -> LoopbackServer {
    LoopbackServer::sequence([
        response(
            json!({"number":17,"title":"Change","state":"open","head":{"ref":"feature","sha":"a".repeat(40)},"base":{"ref":"main","sha":"b".repeat(40)},"created_at":"2026-10-29T00:00:00Z","updated_at":"2026-10-29T00:00:00Z"}),
        ),
        response(json!({"id":23,"state":"COMMENTED","commit_id":"a".repeat(40)})),
    ])
}

fn refresh_review() -> Value {
    bash_tool_call(
        "review",
        &format!(
            "gh pr review 17 -R acme/widget --comment -b 'Blocking finding unchanged' --match-head-commit {}",
            "a".repeat(40)
        ),
    )
}

pub(super) async fn wait_event(audit: &Audit, event: &str) {
    tokio::time::timeout(WAIT, async {
        while audit
            .capture
            .events_where("dekopon_agent", &[("audit.event", event)])
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the scripted turn completes");
}

fn assert_review_requests(api: &LoopbackServer) {
    let calls = api.recorded();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].starts_with(b"GET /api/v3/repos/acme/widget/pulls/17 HTTP/1.1\r\n"));
    assert!(calls[1].starts_with(b"POST /api/v3/repos/acme/widget/pulls/17/reviews HTTP/1.1\r\n"));
    let post = String::from_utf8(calls[1].clone()).unwrap();
    let (_, body) = post.split_once("\r\n\r\n").unwrap();
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["event"], "COMMENT");
    assert_eq!(body["commit_id"], "a".repeat(40));
    for call in calls {
        let call = String::from_utf8(call).unwrap().to_ascii_lowercase();
        assert!(!call.lines().any(|line| line.starts_with("authorization:")));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_matching_synchronize_fires_exactly_once_in_the_anchored_thread_as_the_space_alone() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let root = chat
        .server("acme")
        .user("ualice")
        .post("cteam", "Review anchor")
        .await
        .unwrap();
    let api = review_api();
    let settings = Settings::new(
        vec![subscription(root.id(), "acme/widget", 1)],
        Some(api.url()),
    );
    let address = settings.address;
    let fixture = boot_subscriptions(
        &chat,
        vec![
            meta("reason", "wake_reason", json!({})),
            refresh_review(),
            final_answer("Blocking findings changed."),
        ],
        settings,
    )
    .await;
    deliver(address, "widget", "synchronize", "push-one").await;
    assert_eq!(
        root.thread().next_from("udekopon", WAIT).await.text,
        "Blocking findings changed."
    );
    let decisions = audit.capture.events_where(
        "dekopon_broker",
        &[
            ("audit.event", "broker.decision"),
            ("capability.id", "gh.pull-request.comment"),
            ("decision.point", "invocation"),
        ],
    );
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].field("principal"), Some("team"));
    assert_eq!(decisions[0].field("principal.kind"), Some("space"));
    assert_eq!(decisions[0].field("turn.started_by"), Some("wake"));
    assert_eq!(decisions[0].field("participant.handle"), None);
    assert_eq!(decisions[0].field("asker.allowed"), None);
    assert_eq!(decisions[0].field("conversation.thread"), Some(root.id()));
    let reason: Value = serde_json::from_str(&fixture.tool_result("reason")).unwrap();
    assert_eq!(reason["trigger"], "wake");
    assert!(reason["scheduledBy"].is_null());
    assert_eq!(root.thread().transcript().authors(), ["ualice", "udekopon"]);
    assert_eq!(stored(&fixture).len(), 1);
    assert_review_requests(&api);
    fixture.shutdown().await;
    api.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn unchanged_findings_refresh_the_real_github_review_and_post_nothing_to_chat() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let root = chat
        .server("acme")
        .user("ualice")
        .post("cteam", "Review anchor")
        .await
        .unwrap();
    let api = review_api();
    let settings = Settings::new(
        vec![subscription(root.id(), "acme/widget", 1)],
        Some(api.url()),
    );
    let address = settings.address;
    let fixture = boot_subscriptions(
        &chat,
        vec![
            refresh_review(),
            meta("quiet", "decline_chat_reply", json!({})),
        ],
        settings,
    )
    .await;
    deliver(address, "widget", "synchronize", "quiet-push").await;
    wait_event(&audit, "agent.reply.declined").await;
    let directory = fixture.shutdown().await;
    assert_eq!(root.thread().transcript().authors(), ["ualice"]);
    assert_eq!(
        fs::read_to_string(directory.path().join("wakes.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_review_requests(&api);
    api.join();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unmapped_repository_or_one_outside_route_ids_fires_no_subscription() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let root = chat
        .server("acme")
        .user("ualice")
        .post("cteam", "Review anchor")
        .await
        .unwrap();
    let rows = vec![
        subscription(root.id(), "acme/unmapped", 1),
        subscription(root.id(), "acme/outside", 2),
    ];
    let settings = Settings::new(rows.clone(), None);
    let address = settings.address;
    let fixture = boot_subscriptions(&chat, vec![], settings).await;
    deliver(address, "unmapped", "synchronize", "unmapped-push").await;
    deliver(address, "outside", "synchronize", "outside-push").await;
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 0);
    assert_eq!(stored(&fixture), rows);
    assert!(
        audit
            .capture
            .events_where("dekopon_gatewayd", &[("subscription.operation", "fired")])
            .is_empty()
    );
    assert_eq!(root.thread().transcript().authors(), ["ualice"]);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_delivery_removes_the_subscription_including_a_merged_pr() {
    let _audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let root = chat
        .server("acme")
        .user("ualice")
        .post("cteam", "Review anchor")
        .await
        .unwrap();
    let settings = Settings::new(vec![subscription(root.id(), "acme/widget", 1)], None);
    let address = settings.address;
    let fixture = boot_subscriptions(&chat, vec![], settings).await;
    deliver(address, "widget", "closed", "closed-pr").await;
    assert!(stored(&fixture).is_empty());
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 0);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn another_mapped_participant_can_list_and_cancel_channel_subscriptions_but_a_dm_cannot() {
    let _audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let server = chat.server("acme");
    let root = server
        .user("ualice")
        .post("cteam", "Review anchor")
        .await
        .unwrap();
    let settings = Settings::new(vec![subscription(root.id(), "acme/widget", 1)], None);
    let fixture = boot_subscriptions(
        &chat,
        vec![
            meta("list", "wake", json!({"action":"listSubscriptions"})),
            final_answer("Listed."),
            meta("dm-list", "wake", json!({"action":"listSubscriptions"})),
            meta(
                "dm-cancel",
                "wake",
                json!({"action":"cancelSubscription","id":1}),
            ),
            final_answer("Use the channel."),
            meta(
                "cancel",
                "wake",
                json!({"action":"cancelSubscription","id":1}),
            ),
            final_answer("Cancelled."),
        ],
        settings,
    )
    .await;
    let listed = server
        .user("ubob")
        .post("cteam", "<@udekopon> list subscriptions")
        .await
        .unwrap();
    assert_eq!(
        listed.thread().next_from("udekopon", WAIT).await.text,
        "Listed."
    );
    assert!(fixture.tool_result("list").contains("subscription 1:"));
    let dm = server
        .user("ualice")
        .post("dalice", "list and cancel subscriptions")
        .await
        .unwrap();
    assert_eq!(
        dm.thread().next_from("udekopon", WAIT).await.text,
        "Use the channel."
    );
    for id in ["dm-list", "dm-cancel"] {
        assert_eq!(
            fixture.tool_result(id),
            "Not done: space wakes require an authorized participant in a mapped channel."
        );
    }
    assert_eq!(stored(&fixture).len(), 1);
    let cancelled = server
        .user("ubob")
        .post("cteam", "<@udekopon> cancel subscription 1")
        .await
        .unwrap();
    assert_eq!(
        cancelled.thread().next_from("udekopon", WAIT).await.text,
        "Cancelled."
    );
    assert_eq!(fixture.tool_result("cancel"), "Cancelled subscription 1.");
    assert!(stored(&fixture).is_empty());
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_wake_reports_non_review_capability_work_in_its_anchored_thread() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let root = chat
        .server("acme")
        .user("ualice")
        .post("cteam", "Review anchor")
        .await
        .unwrap();
    let settings = Settings::new(vec![subscription(root.id(), "acme/widget", 1)], None);
    let address = settings.address;
    let fixture = boot_subscriptions(
        &chat,
        vec![
            bash_tool_call("work", "probe upper --text inspected"),
            meta("quiet", "decline_chat_reply", json!({})),
            final_answer("I inspected the PR but could not refresh its review."),
        ],
        settings,
    )
    .await;
    deliver(address, "widget", "synchronize", "unreported-work").await;
    assert_eq!(
        root.thread().next_from("udekopon", WAIT).await.text,
        "I inspected the PR but could not refresh its review."
    );
    let executions = audit.capture.events_where(
        "dekopon_broker",
        &[
            ("audit.event", "broker.execution"),
            ("capability.id", "cli-probe.upper"),
        ],
    );
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].field("outcome"), Some("Succeeded"));
    assert_eq!(root.thread().transcript().authors(), ["ualice", "udekopon"]);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quiet_review_that_also_ran_another_effect_cannot_decline() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let root = chat
        .server("acme")
        .user("ualice")
        .post("cteam", "Review anchor")
        .await
        .unwrap();
    let api = review_api();
    let settings = Settings::new(
        vec![subscription(root.id(), "acme/widget", 1)],
        Some(api.url()),
    );
    let address = settings.address;
    let fixture = boot_subscriptions(
        &chat,
        vec![
            refresh_review(),
            bash_tool_call("other-work", "probe upper --text inspected"),
            meta("quiet", "decline_chat_reply", json!({})),
            final_answer("I refreshed the review and also ran another capability."),
        ],
        settings,
    )
    .await;
    deliver(address, "widget", "synchronize", "review-and-other-work").await;
    assert_eq!(
        root.thread().next_from("udekopon", WAIT).await.text,
        "I refreshed the review and also ran another capability."
    );
    let other = audit.capture.events_where(
        "dekopon_broker",
        &[
            ("audit.event", "broker.execution"),
            ("capability.id", "cli-probe.upper"),
        ],
    );
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].field("outcome"), Some("Succeeded"));
    assert_eq!(root.thread().transcript().authors(), ["ualice", "udekopon"]);
    assert_review_requests(&api);
    fixture.shutdown().await;
    api.join();
}
