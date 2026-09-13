//! A broker with no `telemetry` block records every decision on stdout, as JSON.
//!
//! This is the half of the constitution a library test cannot show: the real binary, its real
//! console layer, and a configuration with no exporter at all. The decision and its outcome have to
//! come out of the process as structured records, because stdout is the only place they go.
//!
//! It is also where offline correlation is pinned. With no exporter there is no OpenTelemetry
//! context, so the formatter fabricates no native `trace_id` — and the record still names the
//! caller's trace, because `broker.invocation` carries it as the ordinary `trace` span field and the
//! JSON formatter renders every enclosing span.

use std::{
    fs,
    io::{BufRead as _, BufReader},
    os::unix::fs::PermissionsExt as _,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use dekopon_broker_protocol::{BrokerClient, FrameLimits, InvocationRequest, TraceParent};
use dekopon_capability::InvocationOutcome;
use dekopon_test_support::provider_fixture;
use serde_json::{Value, json};

/// The trace the client declares it belongs to, and the hex the record has to name.
const CLIENT_TRACE_ID: [u8; 16] = [
    0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47, 0x36,
];
const CLIENT_TRACE_HEX: &str = "4bf92f3577b34da6a3ce929d0e0e4736";

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().expect("inspect child exit").is_none() {
            self.0.kill().expect("stop fixture child");
        }
        self.0.wait().expect("reap fixture child");
    }
}

fn write_owner_only(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("secure fixture");
}

#[tokio::test]
async fn a_broker_without_telemetry_records_every_decision_on_stdout() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let private = directory.path();
    fs::set_permissions(private, fs::Permissions::from_mode(0o700)).expect("private fixture");
    let provider = private.join("cli-probe-provider.wasm");
    fs::copy(provider_fixture("cli-probe-provider.wasm"), &provider)
        .expect("copy provider fixture");
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o600)).expect("secure provider");
    let policies = private.join("policies.cedar");
    write_owner_only(
        &policies,
        br#"@id("caller-upper")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"cli-probe.upper",
       resource == Dekopon::Provider::"cli-probe");"#,
    );
    let socket = private.join("broker.sock");
    let config = private.join("broker.json");
    let uid = dekopon_brokerd::current_uid();
    write_owner_only(
        &config,
        &serde_json::to_vec(&json!({
            // No `telemetry` section: the console is the only sink.
            "apiVersion": dekopon_brokerd::CONFIG_API_VERSION,
            "socketPath": &socket,
            "brokerPrincipal": "broker-test",
            "policyRevision": "policy-test",
            "policiesPath": &policies,
            "providers": [&provider],
            "identities": [{
                "uid": uid,
                "principal": "caller",
                "actor": {"type": "agent", "agent": "brokerd-test"},
            }],
            "constraintSets": {
                "cli-probe.upper": {
                    "provider": "cli-probe",
                    "effect": "read-only",
                    "risk": "Low",
                    "constraints": {"timeoutMs": 30000, "maxOutputBytes": 1048576},
                }
            },
        }))
        .expect("config serializes"),
    );

    let mut broker = Process(
        Command::new(env!("CARGO_BIN_EXE_dekopon-brokerd"))
            .args(["--config", config.to_str().expect("utf-8 fixture path")])
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn the broker"),
    );
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let stdout = broker.0.stdout.take().expect("piped stdout");
    let collected = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            collected.lock().expect("stdout sink").push(line);
        }
    });

    let until = Instant::now() + Duration::from_secs(30);
    while fs::symlink_metadata(&socket).is_err() {
        assert!(
            broker.0.try_wait().expect("inspect broker").is_none(),
            "the broker exited before it bound its socket: {:?}",
            lines.lock().expect("stdout sink")
        );
        assert!(Instant::now() < until, "broker socket readiness deadline");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let client = BrokerClient::new(&socket, uid, FrameLimits::default()).expect("client");
    let result = client
        .invoke(
            None,
            InvocationRequest {
                id: "invoke-stdout".parse().expect("invocation"),
                capability: "cli-probe.upper".parse().expect("capability"),
                trace_parent: TraceParent::new(
                    CLIENT_TRACE_ID,
                    [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7],
                    1,
                )
                .expect("valid W3C parent fixture"),
                secret_use: None,
                input: json!({"text": "hello through broker"}),
            },
        )
        .await
        .expect("the authorized invocation completes");
    assert_eq!(result.outcome, InvocationOutcome::Succeeded);

    let until = Instant::now() + Duration::from_secs(30);
    let records = loop {
        let records = lines
            .lock()
            .expect("stdout sink")
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record["invocation.id"] == "invoke-stdout")
            .collect::<Vec<_>>();
        if records.len() >= 2 {
            break records;
        }
        assert!(
            Instant::now() < until,
            "the broker's stdout carried no audit record: {:?}",
            lines.lock().expect("stdout sink")
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let decision = records
        .iter()
        .find(|record| record["audit.event"] == "broker.decision")
        .expect("the decision reached stdout");
    assert_eq!(decision["capability.id"], "cli-probe.upper");
    assert_eq!(decision["decision.allowed"], true);
    assert_eq!(decision["policy.ids"], "caller-upper");
    assert_eq!(decision["policy.revision"], "policy-test");
    assert!(decision["policy.digest"].is_string(), "{decision}");
    assert_eq!(decision["target"], "dekopon_broker::audit");

    let execution = records
        .iter()
        .find(|record| record["audit.event"] == "broker.execution")
        .expect("the outcome reached stdout");
    assert_eq!(execution["outcome"], "Succeeded");
    assert_eq!(execution["provider"], "cli-probe");
    assert!(execution["output.digest"].is_string(), "{execution}");

    // Documented, not accidental: without a tracer provider there is no native context to read, so
    // the formatter fabricates neither identifier.
    assert!(decision.get("trace_id").is_none(), "{decision}");
    assert!(decision.get("span_id").is_none(), "{decision}");

    // And the record is still in the caller's trace, because the invocation span carries the
    // identifier the client sent rather than one derived from an exporter that is not running.
    for record in &records {
        let invocation = record["spans"]
            .as_array()
            .expect("the formatter renders enclosing spans")
            .iter()
            .find(|span| span["name"] == "broker.invocation")
            .expect("the audit record descends from the invocation span");
        assert_eq!(invocation["trace"], CLIENT_TRACE_HEX, "{record}");
    }
}
