use super::*;
use dekopon_gatewayd::fauxlack::Fauxlack;

const WAIT: Duration = Duration::from_secs(30);

pub(super) fn configure_broker(config: &mut Value, policies: &mut String, servers: &[String]) {
    config["identities"][0]["attestor"]["namespaces"] = json!(["fauxlack"]);
    config["principals"] = json!({
        "alice": {"subjects": ["fauxlack.acme.ualice"]},
        "bob": {"subjects": ["fauxlack.acme.ubob"]},
        "carol": {"subjects": ["fauxlack.acme.ucarol"]},
        "team": {"subjects": ["fauxlack.acme.cteam"]}
    });
    if servers.len() > 1 {
        config["principals"]["galice"] = json!({"subjects": ["fauxlack.globex.ualice"]});
        config["principals"]["gteam"] = json!({"subjects": ["fauxlack.globex.cteam"]});
    }
    config["providers"] = json!([provider("cli-probe"), provider("http-probe")]);
    config["capabilities"]["cli-probe"]["capabilities"]["cli-probe.reverse"] = json!({});
    config["capabilities"]["http-probe"] =
        json!({"constraints": {"timeoutMs": 30_000}, "capabilities": {"http-probe.purge": {}}});
    *policies = String::new();
    for principal in ["alice", "bob", "team", "galice", "gteam"]
        .into_iter()
        .filter(|p| config["principals"].get(p).is_some())
    {
        policies.push_str(&format!(r#"
permit(principal == Dekopon::Principal::"{principal}", action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"{AGENT}") when {{ context.via == "{GATEWAY_PRINCIPAL}" }};
permit(principal == Dekopon::Principal::"{principal}", action == Dekopon::Action::"cli-probe.upper", resource == Dekopon::Provider::"cli-probe") when {{ context.via == "{GATEWAY_PRINCIPAL}" && context.agent == "{AGENT}" }};
"#));
    }
    policies.push_str(&format!(r#"
permit(principal == Dekopon::Principal::"alice", action == Dekopon::Action::"cli-probe.reverse", resource == Dekopon::Provider::"cli-probe") when {{ context.via == "{GATEWAY_PRINCIPAL}" && context.agent == "{AGENT}" }};
permit(principal == Dekopon::Principal::"alice", action == Dekopon::Action::"http-probe.purge", resource == Dekopon::Provider::"http-probe") when {{ context.via == "{GATEWAY_PRINCIPAL}" && context.agent == "{AGENT}" }};
"#));
}

pub(super) fn configure_gateway(config: &mut Value, servers: &[String]) {
    config["transports"] = Value::Array(servers.iter().map(|server| json!({"name": format!("{server}-chat"), "kind": "fauxlack", "server": server, "bot": "udekopon", "indicators": {"mode": "native", "progress": "message"}})).collect());
    config["routes"] = Value::Array(servers.iter().flat_map(|server| [
        json!({"transport": format!("{server}-chat"), "agent": AGENT, "conversation": {"kind": ["channel", "thread"], "container": server, "ids": ["cteam", "crandom"]}, "memory": {"mode": "oneShot"}}),
        json!({"transport": format!("{server}-chat"), "agent": AGENT, "conversation": {"kind": ["directMessage"]}, "memory": {"mode": "oneShot"}}),
    ]).collect());
}

async fn boot_fauxlack(chat: &Fauxlack, responses: Vec<Value>, servers: &[&str]) -> Fixture {
    boot_with(
        temporary(),
        responses,
        None,
        "plain",
        false,
        &Timing {
            fauxlack: Some(chat.clone()),
            fauxlack_servers: servers.iter().map(|s| (*s).into()).collect(),
            ..Timing::default()
        },
    )
    .await
}

fn ran(
    audit: &Audit,
    capability: &str,
    principal: &str,
    subject: &str,
    server: &str,
    id: &str,
    thread: Option<&str>,
    space: Option<&str>,
) {
    let principal = format!("{principal:?}");
    let subject = format!("{subject:?}");
    let server = format!("{server:?}");
    let id = format!("{id:?}");
    let mut fields = vec![
        ("capability.id", capability),
        ("principal", principal.as_str()),
        ("subject", subject.as_str()),
        ("decision.allowed", "true"),
        ("transport.kind", "\"fauxlack\""),
        ("conversation.container", server.as_str()),
        ("conversation.id", id.as_str()),
    ];
    let thread = thread.map(|value| format!("{value:?}"));
    let space = space.map(|value| format!("{value:?}"));
    if let Some(thread) = &thread {
        fields.push(("conversation.thread", thread));
    }
    if let Some(space) = &space {
        fields.push(("space.id", space));
    }
    let decision = audit
        .find("broker.decision", &fields)
        .unwrap_or_else(|| panic!("intended decision: {fields:?}: {:#?}", audit.records()));
    if space.is_none() {
        assert_eq!(field(&decision, "space.id"), None);
    }
    if thread.is_none() {
        assert_eq!(field(&decision, "conversation.thread"), None);
    }
    let decision_id = format!(
        "{:?}",
        field(&decision, "decision.id").expect("decision id")
    );
    audit
        .find(
            "broker.execution",
            &[
                ("decision.id", &decision_id),
                ("principal", &principal),
                ("subject", &subject),
                ("outcome", "Succeeded"),
            ],
        )
        .unwrap_or_else(|| panic!("joined successful execution: {:#?}", audit.records()));
}

#[tokio::test(flavor = "multi_thread")]
async fn sequential_turns_in_a_mapped_thread_each_run_as_their_asker_intersected_with_the_space() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let fixture = boot_fauxlack(
        &chat,
        vec![
            bash_tool_call("a1", "probe upper --text ship | jq -r .text"),
            final_answer("SHIP"),
            bash_tool_call("b1", "probe upper --text wait | jq -r .text"),
            final_answer("WAIT"),
            bash_tool_call("a2", "probe reverse --text ab | jq -r .text"),
            final_answer("Not here."),
            bash_tool_call("a3", "probe reverse --text ab | jq -r .text"),
            final_answer("ba"),
        ],
        &["acme"],
    )
    .await;
    let acme = chat.server("acme");
    let (alice, bob) = (acme.user("ualice"), acme.user("ubob"));
    let root = alice.post("cteam", "<@udekopon> shout ship").await.unwrap();
    let thread = root.thread();
    assert_eq!(thread.next_from("udekopon", WAIT).await.text, "SHIP");
    bob.reply(&root, "<@udekopon> shout wait").await.unwrap();
    assert_eq!(thread.next_from("udekopon", WAIT).await.text, "WAIT");
    alice.reply(&root, "<@udekopon> reverse ab").await.unwrap();
    assert_eq!(thread.next_from("udekopon", WAIT).await.text, "Not here.");
    let dm = alice.post("dalice", "reverse ab").await.unwrap();
    assert_eq!(dm.thread().next_from("udekopon", WAIT).await.text, "ba");
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 8);
    assert_eq!(fixture.tool_result("a1").trim(), "SHIP");
    assert_eq!(fixture.tool_result("b1").trim(), "WAIT");
    assert!(
        fixture.tool_result("a2").contains(
            "requires capability cli-probe.reverse, which is not granted in this session"
        )
    );
    assert_eq!(fixture.tool_result("a3").trim(), "ba");
    assert_eq!(
        thread.transcript().authors(),
        [
            "ualice", "udekopon", "ubob", "udekopon", "ualice", "udekopon"
        ]
    );
    let _directory = fixture.shutdown().await;
    ran(
        &audit,
        "cli-probe.upper",
        "alice",
        "fauxlack.acme.ualice",
        "acme",
        "cteam",
        Some(root.id()),
        Some("team"),
    );
    ran(
        &audit,
        "cli-probe.upper",
        "bob",
        "fauxlack.acme.ubob",
        "acme",
        "cteam",
        Some(root.id()),
        Some("team"),
    );
    ran(
        &audit,
        "cli-probe.reverse",
        "alice",
        "fauxlack.acme.ualice",
        "acme",
        "dalice",
        None,
        None,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unauthorized_participant_is_refused_before_the_observed_control_calls_the_model() {
    let _audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let fixture = boot_fauxlack(&chat, vec![final_answer("control")], &["acme"]).await;
    let server = chat.server("acme");
    let refused = server
        .user("ucarol")
        .post("cteam", "<@udekopon> hi")
        .await
        .unwrap();
    assert_eq!(
        refused.thread().next_from("udekopon", WAIT).await.text,
        "Sorry carol, you don't have permission for this agent."
    );
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 0);
    let control = server
        .user("ualice")
        .post("cteam", "<@udekopon> control")
        .await
        .unwrap();
    assert_eq!(
        control.thread().next_from("udekopon", WAIT).await.text,
        "control"
    );
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 1);
    let _directory = fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bot_authors_and_nonmember_delivery_are_suppressed_before_observed_controls() {
    let _audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let fixture = boot_fauxlack(
        &chat,
        vec![
            final_answer("bot control"),
            final_answer("membership control"),
        ],
        &["acme"],
    )
    .await;
    let server = chat.server("acme");
    let alice = server.user("ualice");
    let bot = server
        .user("uhelper")
        .post("cteam", "<@udekopon> hi")
        .await
        .unwrap();
    let control = alice.post("cteam", "<@udekopon> control").await.unwrap();
    assert_eq!(
        control.thread().next_from("udekopon", WAIT).await.text,
        "bot control"
    );
    assert_eq!(bot.thread().transcript().authors(), ["uhelper"]);
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 1);
    let outside = alice.post("crandom", "<@udekopon> hi").await.unwrap();
    let control = alice.post("cteam", "<@udekopon> control").await.unwrap();
    assert_eq!(
        control.thread().next_from("udekopon", WAIT).await.text,
        "membership control"
    );
    assert_eq!(outside.thread().transcript().authors(), ["ualice"]);
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 2);
    let _directory = fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_servers_keep_identical_ids_in_separate_authority_scopes_and_transcripts() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("two-servers.yaml")).unwrap();
    let fixture = boot_fauxlack(
        &chat,
        vec![
            bash_tool_call("acme", "probe upper --text acme | jq -r .text"),
            final_answer("ACME"),
            bash_tool_call("globex", "probe upper --text globex | jq -r .text"),
            final_answer("GLOBEX"),
        ],
        &["acme", "globex"],
    )
    .await;
    let acme = chat.server("acme");
    let globex = chat.server("globex");
    let first = acme
        .user("ualice")
        .post("cteam", "<@udekopon> hi")
        .await
        .unwrap();
    assert_eq!(
        first.thread().next_from("udekopon", WAIT).await.text,
        "ACME"
    );
    assert!(globex.transcript().messages.is_empty());
    let acme_before = acme.transcript().messages.len();
    let second = globex
        .user("ualice")
        .post("cteam", "<@udekopon> hi")
        .await
        .unwrap();
    assert_eq!(
        second.thread().next_from("udekopon", WAIT).await.text,
        "GLOBEX"
    );
    assert_eq!(acme.transcript().messages.len(), acme_before);
    assert_eq!(
        first.thread().transcript().authors(),
        ["ualice", "udekopon"]
    );
    assert_eq!(
        second.thread().transcript().authors(),
        ["ualice", "udekopon"]
    );
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 4);
    let _directory = fixture.shutdown().await;
    ran(
        &audit,
        "cli-probe.upper",
        "alice",
        "fauxlack.acme.ualice",
        "acme",
        "cteam",
        Some(first.id()),
        Some("team"),
    );
    ran(
        &audit,
        "cli-probe.upper",
        "galice",
        "fauxlack.globex.ualice",
        "globex",
        "cteam",
        Some(second.id()),
        Some("gteam"),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn manifest_files_reach_the_model_and_an_authorized_image_reaches_the_completed_answer() {
    let _audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let fixture = boot_fauxlack(
        &chat,
        vec![
            bash_tool_call("send-image", "httpprobe asset-send chat-asset:2"),
            final_answer("Here is your chart."),
        ],
        &["acme"],
    )
    .await;
    let posted = chat
        .server("acme")
        .user("ualice")
        .post_with("dalice", "send chart", &["notes.txt", "chart.png"])
        .await
        .unwrap();
    let answer = posted.thread().next_from("udekopon", WAIT).await;
    assert_eq!(answer.text, "Here is your chart.");
    assert_eq!(answer.files.len(), 1);
    assert_eq!(answer.files[0].name, "asset-1.png");
    assert_eq!(answer.files[0].mime, "image/png");
    assert_eq!(
        answer.files[0].bytes,
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            "iVBORw0KGgppaXR0eSBwaXhlbHM="
        )
        .unwrap()
    );
    assert!(
        fixture.model_prompts.lock()[0]
            .to_string()
            .contains("[gateway: attached chat-asset:1 — notes.txt]")
    );
    assert!(fixture.tool_result("send-image").contains("true"));
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 2);
    let _directory = fixture.shutdown().await;
}
