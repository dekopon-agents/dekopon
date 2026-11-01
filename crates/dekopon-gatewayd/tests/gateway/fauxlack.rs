use super::*;
use dekopon_gatewayd::fauxlack::Fauxlack;

const WAIT: Duration = Duration::from_secs(30);

pub(super) fn configure_broker(config: &mut Value, policies: &mut String, servers: &[String]) {
    config["identities"][0]["attestor"]["namespaces"] = json!(
        servers
            .iter()
            .map(|server| format!("drn:fauxlack:*:{server}:*"))
            .collect::<Vec<_>>()
    );
    config["principals"] = json!({
        "alice": {"subjects": ["drn:fauxlack:user:acme:ualice"]},
        "bob": {"subjects": ["drn:fauxlack:user:acme:ubob"]},
        "carol": {"subjects": ["drn:fauxlack:user:acme:ucarol"]},
        "team": {"subjects": ["drn:fauxlack:channel:acme:cteam"]}
    });
    if servers.len() > 1 {
        config["principals"]["galice"] = json!({"subjects": ["drn:fauxlack:user:globex:ualice"]});
        config["principals"]["gteam"] = json!({"subjects": ["drn:fauxlack:channel:globex:cteam"]});
    }
    config["providers"] = json!([
        provider("cli-probe"),
        provider("http-probe"),
        provider("clock-raw-probe")
    ]);
    config["capabilities"]["clock-probe"] =
        json!({"constraints": {"timeoutMs": 30_000}, "capabilities": {"clock.now": {}}});
    config["capabilities"]["cli-probe"]["capabilities"]["cli-probe.reverse"] = json!({});
    config["capabilities"]["http-probe"] = json!({"constraints": {"timeoutMs": 30_000, "asset": {"attach": false, "send": true, "remove": false}}, "capabilities": {"http-probe.purge": {}}});
    *policies = String::new();
    for principal in ["alice", "bob", "team", "galice", "gteam"]
        .into_iter()
        .filter(|p| config["principals"].get(p).is_some())
    {
        policies.push_str(&format!(r#"
@id("{principal}-session")
permit(principal == Dekopon::Principal::"{principal}", action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"{AGENT}") when {{ context.via == "{GATEWAY_PRINCIPAL}" }};
@id("{principal}-upper")
permit(principal == Dekopon::Principal::"{principal}", action == Dekopon::Action::"cli-probe.upper", resource == Dekopon::Provider::"cli-probe") when {{ context.via == "{GATEWAY_PRINCIPAL}" && context.agent == "{AGENT}" }};
"#));
    }
    policies.push_str(&format!(r#"
@id("alice-reverse")
permit(principal == Dekopon::Principal::"alice", action == Dekopon::Action::"cli-probe.reverse", resource == Dekopon::Provider::"cli-probe") when {{ context.via == "{GATEWAY_PRINCIPAL}" && context.agent == "{AGENT}" }};
@id("alice-send")
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

struct Scope<'a> {
    server: &'a str,
    id: &'a str,
    thread: Option<&'a str>,
    space: Option<&'a str>,
    kind: &'a str,
}

fn ran(audit: &Audit, capability: &str, principal: &str, subject: &str, scope: Scope<'_>) {
    let Scope {
        server,
        id,
        thread,
        space,
        kind,
    } = scope;
    let kind = format!("{kind:?}");
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
        ("conversation.kind", kind.as_str()),
    ];
    let thread = thread.map(|value| format!("{value:?}"));
    let space = space.map(|value| format!("{value:?}"));
    if let Some(thread) = &thread {
        fields.push(("conversation.thread", thread));
    }
    if let Some(space) = &space {
        fields.push(("space.id", space));
        fields.extend_from_slice(&[("asker.allowed", "true"), ("space.allowed", "true")]);
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
                ("capability.id", capability),
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
    assert_eq!(fixture.tool_result("a1").trim(), "SHIP\n[exit code: 0]");
    assert_eq!(fixture.tool_result("b1").trim(), "WAIT\n[exit code: 0]");
    assert!(
        fixture.tool_result("a2").contains(
            "requires capability cli-probe.reverse, which is not granted in this session"
        )
    );
    assert_eq!(fixture.tool_result("a3").trim(), "ba\n[exit code: 0]");
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
        "drn:fauxlack:user:acme:ualice",
        Scope {
            server: "acme",
            id: "cteam",
            thread: Some(root.id()),
            space: Some("team"),
            kind: "channel",
        },
    );
    ran(
        &audit,
        "cli-probe.upper",
        "bob",
        "drn:fauxlack:user:acme:ubob",
        Scope {
            server: "acme",
            id: "cteam",
            thread: Some(root.id()),
            space: Some("team"),
            kind: "thread",
        },
    );
    ran(
        &audit,
        "cli-probe.reverse",
        "alice",
        "drn:fauxlack:user:acme:ualice",
        Scope {
            server: "acme",
            id: "dalice",
            thread: None,
            space: None,
            kind: "directMessage",
        },
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
        "drn:fauxlack:user:acme:ualice",
        Scope {
            server: "acme",
            id: "cteam",
            thread: Some(first.id()),
            space: Some("team"),
            kind: "channel",
        },
    );
    ran(
        &audit,
        "cli-probe.upper",
        "galice",
        "drn:fauxlack:user:globex:ualice",
        Scope {
            server: "globex",
            id: "cteam",
            thread: Some(second.id()),
            space: Some("gteam"),
            kind: "channel",
        },
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
    assert_eq!(
        answer.files.len(),
        1,
        "tool result: {}",
        fixture.tool_result("send-image")
    );
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
    let first = fixture.model_prompts.lock()[0].clone();
    assert!(first["messages"].as_array().unwrap().iter().any(|message| message["role"] == "user" && message["content"].as_str().is_some_and(|text| text.lines().any(|line| line == "  chat-asset:1 — notes.txt (text/plain, 11 B) — attached to this message"))), "{first:#}");
    assert!(fixture.tool_result("send-image").contains("true"));
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 2);
    let _directory = fixture.shutdown().await;
}

fn config_problems(
    errors: &[dekopon_gatewayd::GatewaydError],
) -> Vec<&dekopon_gatewayd::ConfigProblem> {
    errors
        .iter()
        .flat_map(|error| match error {
            dekopon_gatewayd::GatewaydError::Config(dekopon_gatewayd::ConfigError::Invalid {
                problems,
                ..
            }) => problems.iter().collect(),
            dekopon_gatewayd::GatewaydError::Problems { problems } => config_problems(problems),
            dekopon_gatewayd::GatewaydError::AssetStore { .. }
            | dekopon_gatewayd::GatewaydError::Config(_)
            | dekopon_gatewayd::GatewaydError::Catalog(_)
            | dekopon_gatewayd::GatewaydError::Route(_)
            | dekopon_gatewayd::GatewaydError::Startup { .. }
            | dekopon_gatewayd::GatewaydError::BrokerProbe(_)
            | dekopon_gatewayd::GatewaydError::BrokerGeneration
            | dekopon_gatewayd::GatewaydError::TransportConnect { .. }
            | dekopon_gatewayd::GatewaydError::TransportTask(_)
            | dekopon_gatewayd::GatewaydError::TransportsLost
            | dekopon_gatewayd::GatewaydError::Journal { .. }
            | dekopon_gatewayd::GatewaydError::CacheKeySecret(_)
            | dekopon_gatewayd::GatewaydError::WakeStore(_)
            | dekopon_gatewayd::GatewaydError::Proxy(_)
            | dekopon_gatewayd::GatewaydError::Probe(_) => Vec::new(),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn check_and_service_boot_report_transport_conflicts_together() {
    use dekopon_gatewayd::{ConfigProblem, GatewaydError};
    let directory = temporary();
    let uid = dekopon_gatewayd::current_uid();
    write_owner_only(
        &directory.path().join("dekopon.yaml"),
        catalog_text().as_bytes(),
    );
    let mut config = gateway_config_with(
        directory.path(),
        uid,
        "http://127.0.0.1:1/v1",
        None,
        "plain",
        false,
    );
    configure_gateway(&mut config, &["acme".into()]);
    config["transports"].as_array_mut().unwrap().push(
        json!({"name": "acme-chat", "kind": "fauxlack", "server": "acme", "bot": "udekopon"}),
    );
    let path = directory.path().join("gateway.json");
    write_owner_only(&path, &serde_json::to_vec(&config).unwrap());
    let report = dekopon_gatewayd::check(&path, None, None).await;
    let problems = config_problems(&report.problems);
    assert!(
        problems
            .iter()
            .any(|p| matches!(p, ConfigProblem::FauxlackService { .. }))
    );
    assert!(
        problems
            .iter()
            .any(|p| matches!(p, ConfigProblem::DuplicateTransport { .. }))
    );
    let error = dekopon_gatewayd::run(&path, async {}, 32)
        .await
        .unwrap_err();
    assert!(
        config_problems(&[error])
            .iter()
            .any(|p| matches!(p, ConfigProblem::FauxlackService { .. }))
    );
    config["transports"] = json!([
        {"name": "unknown", "kind": "fauxlack", "server": "unknown", "bot": "udekopon"},
        {"name": "person", "kind": "fauxlack", "server": "acme", "bot": "ualice"},
        {"name": "missing", "kind": "fauxlack", "server": "acme", "bot": "missing"},
        {"name": "duplicate", "kind": "fauxlack", "server": "acme", "bot": "ualice"}
    ]);
    write_owner_only(&path, &serde_json::to_vec(&config).unwrap());
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let error = dekopon_gatewayd::run_fauxlack(&path, async {}, chat, 32)
        .await
        .unwrap_err();
    let errors: [GatewaydError; 1] = [error];
    let problems = config_problems(&errors);
    assert!(
        problems
            .iter()
            .any(|p| matches!(p, ConfigProblem::FauxlackServer { .. }))
    );
    assert!(
        problems
            .iter()
            .any(|p| matches!(p, ConfigProblem::FauxlackNotBot { .. }))
    );
    assert!(
        problems
            .iter()
            .any(|p| matches!(p, ConfigProblem::FauxlackBot { .. }))
    );
    assert!(
        problems
            .iter()
            .any(|p| matches!(p, ConfigProblem::FauxlackDuplicate { .. }))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resident_fauxlack_attachment_answers_after_an_observed_route_reload() {
    let audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let fixture = boot_fauxlack(
        &chat,
        vec![final_answer("before"), final_answer("after")],
        &["acme"],
    )
    .await;
    let alice = chat.server("acme").user("ualice");
    let first = alice.post("cteam", "<@udekopon> before").await.unwrap();
    assert_eq!(
        first.thread().next_from("udekopon", WAIT).await.text,
        "before"
    );
    let path = fixture.directory.path().join("dekopon-gatewayd.json");
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["routes"][0]["limits"] = json!({"maxSteps": 3});
    write_owner_only(&path, &serde_json::to_vec(&document).unwrap());
    assert!(
        audit.wait_for_gateway_event("gateway_reload_applied").await,
        "{:#?}",
        audit.records()
    );
    let second = alice.post("cteam", "<@udekopon> after").await.unwrap();
    assert_eq!(
        second.thread().next_from("udekopon", WAIT).await.text,
        "after"
    );
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 2);
    let _directory = fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutting_down_a_fixture_joins_a_model_with_unconsumed_responses() {
    let _audit = Audit::exclusive().await;
    let fixture = boot(vec![final_answer("never requested")]).await;
    assert_eq!(fixture.model_requests.load(Ordering::SeqCst), 0);
    let _directory = fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ungranted_provider_word_is_refused_in_a_fauxlack_turn() {
    let _audit = Audit::exclusive().await;
    let chat = Fauxlack::from_manifest(include_str!("team.yaml")).unwrap();
    let commands = [
        "httpprobe --help",
        "date --clock-in-run-command",
        "httpprobe fetch --not-valid",
        "date --help",
        "certainly-uninstalled",
        "probe upper --text granted | jq -r .text",
    ];
    let script = commands
        .map(|command| format!("{command}; printf 'status=%s\\n' \"$?\""))
        .join("\n");
    let fixture = boot_fauxlack(
        &chat,
        vec![
            bash_tool_call("refusals", &script),
            final_answer("Checked."),
        ],
        &["acme"],
    )
    .await;
    let root = chat
        .server("acme")
        .user("ubob")
        .post("cteam", "<@udekopon> check the command refusals")
        .await
        .unwrap();
    assert_eq!(
        root.thread().next_from("udekopon", WAIT).await.text,
        "Checked."
    );
    let result = fixture.tool_result("refusals");
    let expected = [
        "dekopon-shell: httpprobe: not granted in this conversation",
        "status=127",
        "dekopon-shell: date: not granted in this conversation",
        "status=127",
        "dekopon-shell: httpprobe: not granted in this conversation",
        "status=127",
        "dekopon-shell: date: not granted in this conversation",
        "status=127",
        "dekopon-shell: certainly-uninstalled: command not found",
        "status=127",
        "GRANTED",
        "status=0",
        "[exit code: 0]",
    ]
    .join("\n");
    assert_eq!(result.trim(), expected);
    println!("fauxlack shell output:\n{result}");
    let _directory = fixture.shutdown().await;
}
