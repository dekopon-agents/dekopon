use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Output},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::{
    collections::BTreeMap, io::BufRead as _, os::unix::fs::PermissionsExt as _, process::Stdio,
    sync::Arc,
};

#[cfg(unix)]
use dekopon_broker::{
    Broker, BrokerLimits, ConstraintCatalog, ConstraintSet, CredentialStore, IdentityDirectory,
    InMemoryAuditLog, PolicyEngine, PolicyWorld,
};
#[cfg(unix)]
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
#[cfg(unix)]
use dekopon_broker_protocol::FrameLimits;
#[cfg(unix)]
use dekopon_brokerd::{BrokerServer, ServerLimits, current_uid};
#[cfg(unix)]
use dekopon_capability::{EffectKind, ExecutionConstraints, HttpConstraints, Idempotency};
#[cfg(unix)]
use dekopon_core::{Actor, AgentId, PrincipalId, RiskLevel};
use serde_json::{Value, json};
#[cfg(unix)]
use tokio::{net::UnixListener, sync::oneshot};

/// One policy engine over exactly the capabilities a broker fixture loads.
#[cfg(unix)]
fn broker_policy<'a>(
    policies: &str,
    principal: &str,
    capabilities: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> PolicyEngine {
    let world = PolicyWorld::new(
        [principal.parse().expect("valid principal fixture")],
        capabilities.into_iter().map(|(capability, provider)| {
            (
                capability.parse().expect("valid capability fixture"),
                provider.parse().expect("valid provider fixture"),
            )
        }),
    )
    .expect("distinct fixtures build a world");
    PolicyEngine::new(policies, &world).expect("fixture policy validates")
}

/// The execution bounds those capabilities run under, which policy can never widen.
#[cfg(unix)]
fn broker_constraints<'a>(
    sets: impl IntoIterator<Item = (&'a str, &'a str, ExecutionConstraints)>,
) -> ConstraintCatalog {
    ConstraintCatalog::new(sets.into_iter().map(|(capability, provider, constraints)| {
        (
            capability.parse().expect("valid capability fixture"),
            ConstraintSet {
                provider: provider.parse().expect("valid provider fixture"),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                idempotency: Idempotency::Idempotent,
                credential: None,
                credential_by_agent: BTreeMap::new(),
                constraints,
            },
        )
    }))
    .expect("distinct capability fixtures build a catalog")
}

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dekopon-run"));
    // Every telemetry flag has an env fallback, so ambient OpenTelemetry configuration on the
    // host would silently flip these subprocesses into export mode and fail unrelated tests.
    for (name, _) in std::env::vars_os() {
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("OTEL_") || name.starts_with("DEKOPON_OTEL_") {
            command.env_remove(name);
        }
    }
    command
}

fn provider_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/providers/echo-provider.wasm")
}

fn imported_provider_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(format!("examples/providers/{name}"))
}

fn run(arguments: &[&str]) -> Output {
    binary()
        .args(arguments)
        .output()
        .expect("dekopon-run process starts")
}

#[test]
fn inspects_the_checked_in_provider_component() {
    let provider = provider_path();
    let output = run(&[
        "inspect",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let manifests: Value = serde_json::from_slice(&output.stdout).expect("manifest JSON parses");
    assert_eq!(manifests[0]["id"], "echo");
    let capability_ids = manifests[0]["capabilities"]
        .as_array()
        .expect("capabilities are an array")
        .iter()
        .map(|capability| capability["id"].as_str().expect("capability ID"))
        .collect::<Vec<_>>();
    assert_eq!(
        capability_ids,
        [
            "echo.echo",
            "echo.reverse",
            "echo.upcase",
            "echo.downcase",
            "echo.ransom-case",
        ]
    );
}

#[test]
fn direct_mode_rejects_every_privileged_importing_provider() {
    for fixture in [
        "http-probe-provider.wasm",
        "jsonplaceholder-provider.wasm",
        "memory-chat-provider.wasm",
        "storage-probe-provider.wasm",
    ] {
        let provider = imported_provider_path(fixture);
        let output = run(&[
            "inspect",
            "--provider",
            provider.to_str().expect("UTF-8 fixture path"),
        ]);

        assert_eq!(output.status.code(), Some(1));
        assert!(
            stderr(&output).contains("could not instantiate provider component"),
            "{}",
            stderr(&output)
        );
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn explicit_broker_mode_uses_authenticated_client_without_loading_components() {
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("temporary broker directory");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("secure broker directory");
    let socket = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).expect("bind broker fixture");
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .expect("secure broker socket");
    let registry = BrokerProviderRegistry::load([provider_path()], BrokerHostLimits::default())
        .await
        .expect("load broker provider");
    let audit = Arc::new(InMemoryAuditLog::new(4).expect("valid audit bound"));
    let actor = Actor::Agent {
        agent: "runner-client-test"
            .parse::<AgentId>()
            .expect("valid agent fixture"),
    };
    let principal = "runner-client"
        .parse::<PrincipalId>()
        .expect("valid principal fixture");
    let broker = Arc::new(
        Broker::new(
            registry,
            "broker-test"
                .parse::<PrincipalId>()
                .expect("valid broker principal"),
            "policy-runner-client".to_owned(),
            broker_policy(
                r#"permit(principal == Dekopon::Principal::"runner-client",
                          action == Dekopon::Action::"echo.echo",
                          resource == Dekopon::Provider::"echo");"#,
                "runner-client",
                [
                    ("echo.echo", "echo"),
                    ("echo.reverse", "echo"),
                    ("echo.upcase", "echo"),
                    ("echo.downcase", "echo"),
                    ("echo.ransom-case", "echo"),
                ],
            ),
            broker_constraints([
                ("echo.echo", "echo", ExecutionConstraints::default()),
                // Deployable but ungranted, so the denial stays a policy decision rather than
                // "nothing knows how to run this".
                ("echo.reverse", "echo", ExecutionConstraints::default()),
            ]),
            CredentialStore::empty(),
            IdentityDirectory::empty(),
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("build broker fixture"),
    );
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        dekopon_brokerd::MappedPeer {
            context: dekopon_broker::AuthenticatedContext::new(principal, actor)
                .expect("bind fixture context"),
            attestor: None,
        },
    );
    let limits = ServerLimits {
        frame: FrameLimits::default(),
        max_connections: 4,
        shutdown_grace: Duration::from_secs(2),
    };
    let server = BrokerServer::new(broker, identities, limits).expect("build server fixture");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let server_task = tokio::spawn(server.serve(listener, async move {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a dropped sender is a shutdown signal like a sent one: this future exists \
                      to complete, and a test that panicked before sending must still stop the \
                      server"
        )]
        let _ = shutdown_receive.await;
    }));
    let socket_text = socket.to_str().expect("UTF-8 socket path").to_owned();
    let uid_text = uid.to_string();

    let capabilities_socket = socket_text.clone();
    let capabilities_uid = uid_text.clone();
    let output = tokio::task::spawn_blocking(move || {
        run(&[
            "broker",
            "capabilities",
            "--socket",
            &capabilities_socket,
            "--server-uid",
            &capabilities_uid,
        ])
    })
    .await
    .expect("capabilities process task exits");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let capabilities: Value =
        serde_json::from_slice(&output.stdout).expect("capabilities output is JSON");
    assert_eq!(capabilities.as_array().map(Vec::len), Some(1));
    assert_eq!(capabilities[0]["capability"]["id"], "echo.echo");

    let trace = directory.path().join("broker-trace.json");
    let trace_text = trace.to_str().expect("UTF-8 trace path").to_owned();
    let output = tokio::task::spawn_blocking(move || {
        run(&[
            "--trace",
            &trace_text,
            "broker",
            "invoke",
            "--socket",
            &socket_text,
            "--server-uid",
            &uid_text,
            "--invocation-id",
            "invoke-runner-client",
            "--trace-id",
            "trace-runner-client",
            "echo.echo",
            "--input",
            r#"{"message":"through broker"}"#,
        ])
    })
    .await
    .expect("invoke process task exits");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let result: Value = serde_json::from_slice(&output.stdout).expect("result output is JSON");
    assert_eq!(result["outcome"], "Succeeded");
    assert_eq!(result["output"], json!({"message": "through broker"}));
    let denied_socket = socket.to_str().expect("UTF-8 socket path").to_owned();
    let denied_uid = uid.to_string();
    let denied = tokio::task::spawn_blocking(move || {
        run(&[
            "broker",
            "invoke",
            "--socket",
            &denied_socket,
            "--server-uid",
            &denied_uid,
            "--invocation-id",
            "invoke-runner-denied",
            "--trace-id",
            "trace-runner-denied",
            "echo.reverse",
            "--input",
            r#"{"message":"not authorized"}"#,
        ])
    })
    .await
    .expect("denied process task exits");
    assert_eq!(denied.status.code(), Some(1), "{}", stderr(&denied));
    let denial: Value = serde_json::from_slice(&denied.stdout).expect("denial output is JSON");
    assert_eq!(denial["outcome"], "Denied");
    assert_eq!(denial["error"], "policy-denied");
    assert_eq!(audit.records().await.len(), 3);
    let trace_json = std::fs::read_to_string(trace).expect("broker trace reads");
    assert!(trace_json.contains("runner.broker.invoke"));
    assert!(!trace_json.contains("through broker"));
    assert!(!trace_json.contains(socket.to_string_lossy().as_ref()));

    shutdown_send.send(()).expect("stop broker fixture");
    server_task
        .await
        .expect("server task exits")
        .expect("server drains cleanly");
}

/// Serves `requests` plaintext HTTP requests on loopback, returning the paths it was asked for.
///
/// The provider reaches this through the broker's HTTP host, so a request arriving here is proof
/// the whole chain ran: script, broker authorization, Wasm provider, real socket.
#[cfg(unix)]
fn mock_http_target(requests: usize) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback target binds");
    let authority = format!(
        "127.0.0.1:{}",
        listener.local_addr().expect("target address").port()
    );
    let handle = thread::spawn(move || {
        let mut paths = Vec::new();
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().expect("target accepts");
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("target read timeout configures");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            while find_bytes(&bytes, b"\r\n\r\n").is_none() {
                let count = stream.read(&mut buffer).expect("target request reads");
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8_lossy(&bytes).into_owned();
            if let Some(line) = request.lines().next() {
                paths.push(
                    line.split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
            let body = br#"{"ok":true}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("target headers write");
            stream.write_all(body).expect("target body writes");
            stream.flush().expect("target response flushes");
        }
        paths
    });
    (authority, handle)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn prompt_reaches_http_capabilities_through_the_broker_leg() {
    // The end-to-end proof this phase exists for. Direct mode's linker is import-free and provably
    // cannot perform I/O, so an HTTP-capable capability is reachable only over the broker. One
    // model-authored script drives both legs: `http-probe.fetch` and `curl` go through the broker,
    // `echo.upcase` stays local, and none of it involves a tool schema per capability.
    let (authority, target) = mock_http_target(3);
    let uid = current_uid();
    let directory = tempfile::tempdir().expect("temporary broker directory");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("secure broker directory");
    let socket = directory.path().join("broker.sock");
    let listener = UnixListener::bind(&socket).expect("bind broker fixture");
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .expect("secure broker socket");

    let registry = BrokerProviderRegistry::load(
        [imported_provider_path("http-probe-provider.wasm")],
        BrokerHostLimits::default(),
    )
    .await
    .expect("load HTTP-importing provider into the broker");
    let audit = Arc::new(InMemoryAuditLog::new(16).expect("valid audit bound"));
    let actor = Actor::Agent {
        agent: "prompt-broker-test"
            .parse::<AgentId>()
            .expect("valid agent fixture"),
    };
    let principal = "prompt-broker"
        .parse::<PrincipalId>()
        .expect("valid principal fixture");
    let broker = Arc::new(
        Broker::new(
            registry,
            "broker-test"
                .parse::<PrincipalId>()
                .expect("valid broker principal"),
            "policy-prompt-broker".to_owned(),
            broker_policy(
                r#"permit(principal == Dekopon::Principal::"prompt-broker",
                          action == Dekopon::Action::"http-probe.fetch",
                          resource == Dekopon::Provider::"http-probe");"#,
                "prompt-broker",
                [("http-probe.fetch", "http-probe")],
            ),
            broker_constraints([(
                "http-probe.fetch",
                "http-probe",
                ExecutionConstraints {
                    timeout_ms: 5_000,
                    max_output_bytes: 64 * 1024,
                    http: Some(HttpConstraints {
                        allowed_hosts: vec![authority.clone()],
                        allowed_methods: vec!["GET".to_owned()],
                        max_requests: 1,
                        max_request_bytes: 64 * 1024,
                        max_response_bytes: 64 * 1024,
                        allow_plaintext_loopback: true,
                    }),
                    storage: None,
                },
            )]),
            CredentialStore::empty(),
            IdentityDirectory::empty(),
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("build broker fixture"),
    );
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        dekopon_brokerd::MappedPeer {
            context: dekopon_broker::AuthenticatedContext::new(principal, actor)
                .expect("bind fixture context"),
            attestor: None,
        },
    );
    let server = BrokerServer::new(
        broker,
        identities,
        ServerLimits {
            frame: FrameLimits::default(),
            max_connections: 8,
            shutdown_grace: Duration::from_secs(5),
        },
    )
    .expect("build server fixture");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let server_task = tokio::spawn(server.serve(listener, async move {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a dropped sender is a shutdown signal like a sent one: this future exists \
                      to complete, and a test that panicked before sending must still stop the \
                      server"
        )]
        let _ = shutdown_receive.await;
    }));

    let script = format!(
        "total=0\n\
         for path in alpha beta; do\n\
         status=$(http-probe.fetch --uri \"http://{authority}/$path\" | jq -r .status)\n\
         echo \"$path=$status\"\n\
         total=$(( total + 1 ))\n\
         done\n\
         curl \"http://{authority}/gamma\" | jq -r .status\n\
         echo.upcase --message \"fetched $total\" | jq -r .message"
    );
    let endpoint_listener = TcpListener::bind("127.0.0.1:0").expect("mock endpoint binds");
    let endpoint_address = endpoint_listener
        .local_addr()
        .expect("mock endpoint address");
    let model_server = thread::spawn(move || {
        let (first, first_stream) = read_request(&endpoint_listener);
        let tools = first["tools"].as_array().expect("tools are an array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "bash");
        respond(first_stream, &bash_tool_call("call-1", &script));

        let (second, second_stream) = read_request(&endpoint_listener);
        let content = tool_result(&second);
        respond(second_stream, &final_answer("Fetched three paths."));
        content
    });

    let socket_text = socket.to_str().expect("UTF-8 socket path").to_owned();
    let uid_text = uid.to_string();
    let endpoint = format!("http://{endpoint_address}/v1");
    let provider = provider_path();
    let output = tokio::task::spawn_blocking(move || {
        run(&[
            "prompt",
            "--provider",
            provider.to_str().expect("UTF-8 fixture path"),
            "--broker",
            "--socket",
            &socket_text,
            "--server-uid",
            &uid_text,
            "--curl-capability",
            "http-probe.fetch",
            "--model",
            "test-model",
            "--endpoint",
            &endpoint,
            "--api-key-env",
            "DEKOPON_RUN_TEST_NO_API_KEY",
            "Fetch alpha, beta and gamma",
        ])
    })
    .await
    .expect("prompt process task exits");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout UTF-8"),
        "Fetched three paths.\n"
    );

    // What the model saw: two broker-backed capability calls, one broker-backed `curl`, and one
    // direct-mode capability, all from a single tool call.
    let content = model_server.join().expect("mock endpoint completes");
    assert_eq!(
        content,
        "alpha=200\nbeta=200\n200\nFETCHED 2\n[exit code: 0]"
    );

    // The requests genuinely left the process through the broker's HTTP host.
    let paths = target.join().expect("loopback target completes");
    assert_eq!(paths, vec!["/alpha", "/beta", "/gamma"]);

    // ...and the broker durably audited each one: an authorization decision plus a terminal
    // execution record per invocation, three invocations, six records.
    let records = audit.records().await;
    assert_eq!(records.len(), 6);

    // Every record carries an identifier this session generated, and all of them share one trace,
    // so an operator can recover exactly what one prompt session did from the audit log alone.
    let audited = serde_json::to_value(&records).expect("audit records serialize");
    let invocations = audited
        .as_array()
        .expect("audit records are an array")
        .iter()
        .filter_map(|record| record["event"]["invocation"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(invocations.len(), 6, "{audited}");
    let trace = invocations[0]
        .rsplit_once('-')
        .expect("invocation identifiers extend the session trace")
        .0
        .to_owned();
    assert!(trace.starts_with("dekopon-run-prompt-"), "{trace}");
    assert!(
        invocations
            .iter()
            .all(|invocation| invocation.starts_with(&trace)),
        "{invocations:?}"
    );
    // Three distinct invocation identifiers: the broker rejects a replayed one, so a script that
    // calls the same capability in a loop must not collide with itself.
    let mut unique = invocations.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 3, "{invocations:?}");

    shutdown_send.send(()).expect("stop broker fixture");
    server_task
        .await
        .expect("server task exits")
        .expect("server drains cleanly");
}

#[test]
fn invokes_and_times_the_checked_in_provider_component() {
    let provider = provider_path();
    let output = run(&[
        "invoke",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "echo.echo",
        "--input",
        r#"{"message":"hello"}"#,
        "--repeat",
        "2",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).expect("report JSON parses");
    assert_eq!(report["provider"], "echo");
    assert_eq!(report["iterations"], 2);
    assert_eq!(report["output"], json!({"message": "hello"}));
    assert!(report["timing"]["totalMs"].as_f64().is_some());
}

#[test]
fn invokes_a_checked_in_text_transform() {
    let provider = provider_path();
    let output = run(&[
        "invoke",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "echo.ransom-case",
        "--input",
        r#"{"message":"Hello, World!"}"#,
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).expect("report JSON parses");
    assert_eq!(report["output"], json!({"message": "hElLo, WoRlD!"}));
}

#[test]
fn exports_a_chrome_trace_with_runner_and_provider_spans() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let trace = directory.path().join("trace.json");
    let provider = provider_path();
    let output = run(&[
        "--trace",
        trace.to_str().expect("UTF-8 trace path"),
        "invoke",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "echo.echo",
        "--input",
        "{}",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let events: Vec<Value> =
        serde_json::from_slice(&std::fs::read(trace).expect("trace file reads"))
            .expect("trace is valid JSON");
    let names = events
        .iter()
        .filter_map(|event| event["name"].as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"runner.command"));
    assert!(names.contains(&"runner.invoke"));
    assert!(names.contains(&"runner.provider_invocation"));
    assert!(names.contains(&"provider.compile"));
    assert!(names.contains(&"provider.invoke"));
}

/// `--repeat` is a benchmark, and a benchmark must not bill the sink for its iteration count.
///
/// The JSON report already aggregates the timings, so the log stream carries the first iteration
/// and one summary instead of one record per loop pass — a difference of 9,998 records at
/// `--repeat 10000`.
#[test]
fn repeat_emits_one_iteration_event_and_one_summary_rather_than_one_per_pass() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let trace = directory.path().join("trace.json");
    let provider = provider_path();
    let output = run(&[
        "--trace",
        trace.to_str().expect("UTF-8 trace path"),
        "invoke",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "echo.echo",
        "--input",
        "{}",
        "--repeat",
        "5",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let events: Vec<Value> =
        serde_json::from_slice(&std::fs::read(trace).expect("trace file reads"))
            .expect("trace is valid JSON");
    let audit = events
        .iter()
        .filter_map(|event| event["args"]["audit.event"].as_str())
        .map(|event| event.trim_matches('"').to_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        audit,
        vec![
            "guest.invocation.completed".to_owned(),
            "guest.invocation.summary".to_owned()
        ]
    );
    // The per-iteration spans stay: they are the timing record, and a span costs no log record.
    let invocation_spans = events
        .iter()
        .filter(|event| event["name"] == "runner.provider_invocation" && event["ph"] == "B")
        .count();
    assert_eq!(invocation_spans, 5);
}

/// A local trace file is a sink like any other: the payload opt-in has to reach it, or the flag
/// silently does nothing whenever no OTLP endpoint is configured.
#[test]
fn the_chrome_trace_honors_the_payload_opt_in_without_an_otlp_endpoint() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let trace = directory.path().join("trace.json");
    let provider = provider_path();
    let output = run(&[
        "--trace",
        trace.to_str().expect("UTF-8 trace path"),
        "--otel-telemetry-payloads",
        "true",
        "shell",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "helper_MODEL_AUTHORED() { echo inner; }\nhelper_MODEL_AUTHORED",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let raw = std::fs::read_to_string(&trace).expect("trace file reads");
    assert!(
        raw.contains("helper_MODEL_AUTHORED"),
        "the payload opt-in was dropped on the endpoint-free path"
    );
    assert!(
        !raw.contains("<withheld>"),
        "a payload-gated field stayed withheld with payloads enabled"
    );
}

/// A configured endpoint is part of the command contract, so undelivered telemetry fails the run.
///
/// Reporting success here would tell an operator that a guest execution was fully observed when
/// its spans never left the process.
#[test]
fn configured_otlp_delivery_failure_makes_the_command_fail() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve an unused local port");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("reserved local address")
    );
    drop(listener);
    let provider = provider_path();
    let output = run(&[
        "--otlp-endpoint",
        &endpoint,
        "--otel-export-timeout-ms",
        "100",
        "invoke",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "echo.echo",
        "--input",
        "{}",
    ]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("could not flush OTLP telemetry"),
        "{}",
        stderr(&output)
    );
}

/// An unset endpoint must engage no telemetry path at all.
///
/// This is the property that keeps the feature opt-in: with no `--otlp-endpoint`, the runner is
/// byte-for-byte the runner that shipped before it, including on the failure paths where the new
/// shutdown step could otherwise turn a clean exit code into a 1. Both stderr assertions are
/// exact, not substring checks: the lifecycle audit events are emitted at info/error level, and
/// an exact comparison is what proves they never reach the operator's stderr stream.
#[test]
fn unset_otlp_endpoint_leaves_command_behavior_unchanged() {
    let provider = provider_path();
    let output = run(&[
        "invoke",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "echo.echo",
        "--input",
        "{}",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stderr(&output), "", "success stderr must stay empty");

    let missing = run(&[
        "invoke",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "echo.nonexistent",
        "--input",
        "{}",
    ]);

    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(
        stderr(&missing),
        "error: no loaded provider implements capability echo.nonexistent\n",
        "failure stderr must stay the single classic error line"
    );
}

/// Builds the assistant turn that calls the one scripting tool.
fn bash_tool_call(id: &str, script: &str) -> Value {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": "bash",
                        "arguments": json!({ "script": script }).to_string()
                    }
                }]
            }
        }]
    })
}

/// Builds the assistant turn that ends a session.
fn final_answer(text: &str) -> Value {
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": text, "tool_calls": [] }
        }]
    })
}

/// Returns the content of the first tool result a request carried back to the model.
fn tool_result(request: &Value) -> String {
    request["messages"]
        .as_array()
        .expect("messages are an array")
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("tool result is returned to the model")
        .to_owned()
}

#[test]
fn runs_an_openai_compatible_prompt_tool_loop() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock endpoint binds");
    let address = listener.local_addr().expect("mock endpoint address");
    let server = thread::spawn(move || {
        let (first, first_stream) = read_request(&listener);
        assert_eq!(first["model"], "test-model");

        // The whole point of this phase: one tool, whatever the provider offers. The echo provider
        // exposes five capabilities and the model still sees a single schema.
        let tools = first["tools"].as_array().expect("tools are an array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "bash");
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["script"]["type"],
            "string"
        );
        assert_eq!(
            tools[0]["function"]["parameters"]["required"],
            json!(["script"])
        );
        let description = tools[0]["function"]["description"]
            .as_str()
            .expect("tool description is a string");
        assert!(description.contains("cap --list"), "{description}");

        respond(
            first_stream,
            &bash_tool_call(
                "call-1",
                "echo.upcase --message hello | jq -r .message\ncap --list | jq -r '.[0]'",
            ),
        );

        let (second, second_stream) = read_request(&listener);
        let tool_message = second["messages"]
            .as_array()
            .expect("messages are an array")
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("tool result is returned to model");
        assert_eq!(tool_message["tool_call_id"], "call-1");
        // Combined output plus an exit-code trailer, exactly what `dekopon-run shell` prints.
        assert_eq!(
            tool_message["content"],
            "HELLO\necho.downcase\n[exit code: 0]"
        );
        respond(second_stream, &final_answer("The script printed HELLO."));
    });

    let provider = provider_path();
    let endpoint = format!("http://{address}/v1");
    let output = run(&[
        "prompt",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "--model",
        "test-model",
        "--endpoint",
        &endpoint,
        "--api-key-env",
        "DEKOPON_RUN_TEST_NO_API_KEY",
        "Upcase hello",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout UTF-8"),
        "The script printed HELLO.\n"
    );
    server.join().expect("mock endpoint completes");
}

#[test]
fn prompt_scripts_stay_inside_the_session_capability_ceiling() {
    // The interpreter's ceiling bounds one script; in prompt mode it has to bound the session, or
    // a model widens its own budget just by writing another script.
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock endpoint binds");
    let address = listener.local_addr().expect("mock endpoint address");
    let server = thread::spawn(move || {
        let (_first, first_stream) = read_request(&listener);
        respond(
            first_stream,
            &bash_tool_call("call-1", "echo.echo --n 1\necho.echo --n 2\necho done"),
        );

        let (second, second_stream) = read_request(&listener);
        let content = tool_result(&second);
        assert!(content.contains("capability calls"), "{content}");
        assert!(content.ends_with("[exit code: 2]"), "{content}");
        assert!(!content.contains("done"), "{content}");
        respond(second_stream, &final_answer("I ran out of budget."));

        // A second script gets whatever the first left, which is nothing.
        let (_third, third_stream) = read_request(&listener);
        respond(third_stream, &final_answer("unused"));
    });

    let provider = provider_path();
    let endpoint = format!("http://{address}/v1");
    let output = run(&[
        "prompt",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "--shell-max-capability-calls",
        "1",
        "--model",
        "test-model",
        "--endpoint",
        &endpoint,
        "--api-key-env",
        "DEKOPON_RUN_TEST_NO_API_KEY",
        "Call echo twice",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout UTF-8"),
        "I ran out of budget.\n"
    );
    drop(server);
}

#[test]
fn prompt_scripts_never_read_the_real_process_environment() {
    // Prompt mode gained a broker leg configured partly from the environment. A script that could
    // read `DEKOPON_BROKER_SOCKET` back out would undo the interpreter's central guarantee.
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock endpoint binds");
    let address = listener.local_addr().expect("mock endpoint address");
    let server = thread::spawn(move || {
        let (_first, first_stream) = read_request(&listener);
        respond(
            first_stream,
            &bash_tool_call("call-1", r#"echo "[$DEKOPON_BROKER_SOCKET][$PATH]""#),
        );

        let (second, second_stream) = read_request(&listener);
        let content = tool_result(&second);
        assert_eq!(content, "[][]\n[exit code: 0]");
        assert!(!content.contains("leaked"), "{content}");
        respond(second_stream, &final_answer("Nothing leaked."));
    });

    let provider = provider_path();
    let endpoint = format!("http://{address}/v1");
    let output = binary()
        .args([
            "prompt",
            "--provider",
            provider.to_str().expect("UTF-8 fixture path"),
            "--model",
            "test-model",
            "--endpoint",
            &endpoint,
            "--api-key-env",
            "DEKOPON_RUN_TEST_NO_API_KEY",
            "Read the environment",
        ])
        .env("DEKOPON_BROKER_SOCKET", "/tmp/leaked-broker.sock")
        .output()
        .expect("dekopon-run process starts");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    server.join().expect("mock endpoint completes");
}

#[test]
fn prompt_refuses_broker_connection_flags_without_the_broker_opt_in() {
    // Silently ignoring `--socket` would let an operator believe the broker leg was live and read
    // a "command not found" as a denial rather than as a broker that was never contacted. Every
    // connection flag counts, including the two that used to carry clap defaults: a value that
    // parses and then configures nothing is the exact failure this refusal exists to prevent.
    let provider = provider_path();
    for flag in [
        ["--socket", "/run/dekopon/broker.sock"],
        ["--server-uid", "1000"],
        ["--max-frame-bytes", "4096"],
        ["--io-timeout-ms", "500"],
    ] {
        let output = run(&[
            "prompt",
            "--provider",
            provider.to_str().expect("UTF-8 fixture path"),
            flag[0],
            flag[1],
            "--model",
            "test-model",
            "Do something",
        ]);

        assert_eq!(output.status.code(), Some(1), "{}", flag[0]);
        assert!(
            stderr(&output).contains("--broker"),
            "{}: {}",
            flag[0],
            stderr(&output)
        );
    }
}

fn read_request(listener: &TcpListener) -> (Value, TcpStream) {
    let (mut stream, _) = listener.accept().expect("mock endpoint accepts");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("mock read timeout configures");
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let count = stream.read(&mut buffer).expect("mock request reads");
        assert_ne!(count, 0, "connection closed before request headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = find_bytes(&bytes, b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).expect("headers UTF-8");
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| {
                value
                    .trim()
                    .parse::<usize>()
                    .expect("content length is numeric")
            })
        })
        .expect("content length header");
    while bytes.len() - header_end < content_length {
        let count = stream.read(&mut buffer).expect("mock body reads");
        assert_ne!(count, 0, "connection closed before request body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = &bytes[header_end..header_end + content_length];
    let value = serde_json::from_slice(body).expect("request body is JSON");
    (value, stream)
}

fn respond(mut stream: TcpStream, body: &Value) {
    let body = serde_json::to_vec(body).expect("response serializes");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("response headers write");
    stream.write_all(&body).expect("response body writes");
    stream.flush().expect("response flushes");
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Exact standard output, borrowed so a test can assert on both streams of one `Output`.
#[cfg(unix)]
fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("standard output is UTF-8")
}

// ---------------------------------------------------------------------------
// Sandboxed shell subcommand
// ---------------------------------------------------------------------------

/// Runs one script against the exact fetched echo provider, returning stdout and the exit code.
fn shell(script: &str, extra: &[&str]) -> (String, i32) {
    let provider = provider_path();
    let mut arguments = vec![
        "shell",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
    ];
    arguments.extend_from_slice(extra);
    arguments.push(script);
    let output = run(&arguments);
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.code().expect("the runner exits normally"),
    )
}

#[test]
fn shell_runs_control_flow_and_reports_the_exit_code() {
    let (stdout, code) = shell(
        "total=0\nfor n in 1 2 3; do total=$(( total + n )); done\nif [ $total -eq 6 ]; then echo \"sum=$total\"; fi",
        &[],
    );
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(stdout, "sum=6\n[exit code: 0]\n");
}

#[test]
fn shell_invokes_a_granted_capability_and_pipes_it_through_jq() {
    let (stdout, code) = shell("echo.upcase --message hello | jq -r .message", &[]);
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(stdout, "HELLO\n[exit code: 0]\n");
}

#[test]
fn shell_propagates_capability_exit_codes_through_and_or_lists() {
    let (stdout, code) = shell(
        "echo.echo --message hi > result && echo ok || echo failed\necho $?",
        &[],
    );
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("ok"), "{stdout}");

    let (stdout, code) = shell("not.granted || echo recovered", &[]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("command not found"), "{stdout}");
    assert!(stdout.contains("recovered"), "{stdout}");
}

#[test]
fn shell_reports_an_ungranted_capability_as_command_not_found() {
    let (stdout, code) = shell("not.granted --x 1", &[]);
    assert_eq!(code, 127, "{stdout}");
    assert!(stdout.ends_with("[exit code: 127]\n"), "{stdout}");
}

#[test]
fn shell_lists_and_describes_capabilities_through_the_escape_hatch() {
    let (stdout, code) = shell("cap --list | jq -r '.[0]'", &[]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("echo.downcase"), "{stdout}");

    let (stdout, code) = shell("cap --describe echo.echo | jq -r .capability", &[]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("echo.echo"), "{stdout}");
}

#[test]
fn shell_rejects_every_loudly_dropped_grammar_feature_rather_than_ignoring_it() {
    // An unquoted `*` is an ordinary character: there is no filesystem to glob against, so this
    // belongs to the documented "inert literal" group rather than the rejected one.
    let (stdout, code) = shell("echo *", &[]);
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(stdout, "*\n[exit code: 0]\n");

    // Everything below fails by name instead of doing something the script did not ask for.
    for (script, expected, forbidden) in [
        ("echo pwned &", "backgrounding", "pwned"),
        ("eval 'echo pwned'", "eval", "pwned"),
        ("echo `echo pwned`", "backtick", "pwned"),
        ("set -x\necho pwned", "option -x is not supported", "pwned"),
        ("echo pwned 3>/dev/null", "only descriptors 1", "pwned"),
        ("[[ abc =~ a.c ]] && echo pwned", "regex matching", "pwned"),
        (
            "f=a.json\n[[ $f == *.json ]] && echo pwned",
            "glob in bash",
            "pwned",
        ),
    ] {
        let (stdout, code) = shell(script, &[]);
        assert_eq!(code, 2, "{script}: {stdout}");
        assert!(stdout.contains(expected), "{script}: {stdout}");
        assert!(!stdout.contains(forbidden), "{script}: {stdout}");
    }

    // The three options that *are* enforced work end to end, and `set -e` stops the script where
    // it says it will.
    let (stdout, code) = shell("set -euo pipefail\nnosuchcmd.here | jq .\necho pwned", &[]);
    assert_eq!(code, 127, "{stdout}");
    assert!(!stdout.contains("pwned"), "{stdout}");

    // The two streams a script *can* address end up in the one combined transcript a terminal
    // would have shown, so an operator replaying a model's script sees what the model saw.
    let (stdout, code) = shell("echo kept\necho noted >&2\nnosuchcmd.here 2>/dev/null", &[]);
    assert_eq!(code, 127, "{stdout}");
    assert!(stdout.contains("kept"), "{stdout}");
    assert!(stdout.contains("noted"), "{stdout}");
    assert!(!stdout.contains("command not found"), "{stdout}");
}

#[test]
fn shell_limits_trip_with_their_documented_exit_codes() {
    let (stdout, code) = shell("while true; do x=1; done", &["--shell-max-steps", "400"]);
    assert_eq!(code, 2, "{stdout}");
    assert!(stdout.contains("step budget exhausted"), "{stdout}");

    let (stdout, code) = shell(
        "recurse() { recurse; }\nrecurse",
        &["--shell-max-recursion-depth", "8"],
    );
    assert_eq!(code, 2, "{stdout}");
    assert!(stdout.contains("nested deeper"), "{stdout}");

    let (stdout, code) = shell(
        "for n in 1 2 3 4 5; do echo.echo --n $n; done",
        &["--shell-max-capability-calls", "2"],
    );
    assert_eq!(code, 2, "{stdout}");
    assert!(stdout.contains("capability calls"), "{stdout}");

    let (stdout, code) = shell("sleep 30", &["--shell-timeout-ms", "50"]);
    assert_eq!(code, 124, "{stdout}");
    assert!(stdout.contains("deadline"), "{stdout}");

    let (stdout, code) = shell(
        "n=0\nwhile [ $n -lt 40 ]; do echo line-$n; n=$(( n + 1 )); done",
        &["--shell-max-output-lines", "8"],
    );
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains("Output truncated (40 total lines)"),
        "{stdout}"
    );
    assert!(stdout.starts_with("line-0\n"), "{stdout}");
    assert!(stdout.contains("line-39\n"), "{stdout}");
}

#[test]
fn shell_never_reads_the_real_process_environment() {
    // Set a genuine environment variable on the child process, then prove the script cannot read
    // it. The interpreter's namespace is seeded only by the script's own assignments.
    let provider = provider_path();
    let output = binary()
        .args([
            "shell",
            "--provider",
            provider.to_str().expect("UTF-8 fixture path"),
            r#"echo "[$DEKOPON_SHELL_LEAK_PROBE][$PATH]""#,
        ])
        .env("DEKOPON_SHELL_LEAK_PROBE", "leaked-secret")
        .output()
        .expect("dekopon-run process starts");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert_eq!(stdout, "[][]\n[exit code: 0]\n");
    assert!(!stdout.contains("leaked-secret"), "{stdout}");
}

#[test]
fn shell_curl_is_command_not_found_without_a_configured_capability() {
    let (stdout, code) = shell("curl https://example.test/", &[]);
    assert_eq!(code, 127, "{stdout}");
    assert!(stdout.contains("command not found"), "{stdout}");
}

#[test]
fn shell_exit_status_wraps_like_bash() {
    let (stdout, code) = shell("exit 300", &[]);
    assert_eq!(code, 44, "{stdout}");
    assert_eq!(stdout, "[exit code: 44]\n");
}

#[test]
fn shell_named_buffers_round_trip_without_touching_the_filesystem() {
    let (stdout, code) = shell("echo hi > buf\necho there >> buf\ncat buf | wc -l", &[]);
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(stdout, "2\n[exit code: 0]\n");

    let (stdout, code) = shell("cat /etc/hosts", &[]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("no such buffer"), "{stdout}");
}

#[test]
fn shell_bounds_value_memory_not_only_operation_counts() {
    // Doubling a string is one cheap step and twice the memory, so every ceiling that counts
    // operations leaves memory unbounded. This trips in a few hundred steps of a 100,000 budget.
    let (stdout, code) = shell(
        "x=aaaaaaaaaaaaaaaa\ni=0\nwhile [ $i -lt 30 ]; do x=\"$x$x\"; i=$(( i + 1 )); done\necho survived",
        &["--shell-max-value-bytes", "65536"],
    );
    assert_eq!(code, 2, "{stdout}");
    assert!(stdout.contains("bytes of values"), "{stdout}");
    assert!(!stdout.contains("survived"), "{stdout}");
}

#[test]
fn shell_refuses_input_that_would_overflow_the_parser_stack() {
    // A stack overflow is a SIGABRT, not a catchable panic: the runner would die before printing
    // an exit code at all. These have to come back as ordinary syntax errors.
    for script in [
        format!("echo $(( {}1{} ))", "(".repeat(4_000), ")".repeat(4_000)),
        format!("echo {}echo hi{}", "$(".repeat(2_000), ")".repeat(2_000)),
    ] {
        let (stdout, code) = shell(&script, &[]);
        assert_eq!(code, 2, "{stdout}");
        assert!(stdout.contains("syntax error"), "{stdout}");
    }
}

#[test]
fn shell_jq_cannot_read_the_real_process_environment() {
    // `jq` embeds jaq, whose standard library exports an `env` filter reading the host process
    // environment. That path bypasses `$VAR` lookup entirely, so the guard needs its own test:
    // a script could otherwise dump every host secret and post it through `curl`.
    let provider = provider_path();
    let output = binary()
        .args([
            "shell",
            "--provider",
            provider.to_str().expect("UTF-8 fixture path"),
            r#"jq -r "env.DEKOPON_SHELL_LEAK_PROBE""#,
        ])
        .env("DEKOPON_SHELL_LEAK_PROBE", "leaked-secret")
        .output()
        .expect("dekopon-run process starts");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(!stdout.contains("leaked-secret"), "{stdout}");
    assert!(stdout.contains("undefined filter"), "{stdout}");
}

#[test]
fn shell_runs_case_branches_and_here_documents() {
    let (stdout, code) = shell(
        "for w in ready broken other; do case $w in ready) echo go ;; broken|failed) echo stop ;; *) echo huh ;; esac; done\ncat <<EOF\ndone\nEOF",
        &[],
    );
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(stdout, "go\nstop\nhuh\ndone\n[exit code: 0]\n");
}

#[test]
fn shell_rejects_a_case_pattern_that_would_glob() {
    // Matching `*.json` as four literal characters is the silent wrong answer this shell refuses.
    let (stdout, code) = shell("case a.json in *.json) echo hit ;; esac", &[]);
    assert_eq!(code, 2, "{stdout}");
    assert!(stdout.contains("literal text"), "{stdout}");
}

#[test]
fn shell_reads_the_clock_only_when_an_operator_allows_it() {
    let (denied, code) = shell("date +%s", &[]);
    assert_eq!(code, 127, "{denied}");
    assert!(denied.contains("command not found"), "{denied}");
    assert!(denied.contains("--shell-allow-clock"), "{denied}");

    let (allowed, code) = shell("date +%s", &["--shell-allow-clock"]);
    assert_eq!(code, 0, "{allowed}");
    let seconds = allowed
        .lines()
        .next()
        .expect("an epoch second")
        .parse::<i64>()
        .expect("the epoch second is a number");
    assert!(seconds > 1_577_836_800, "{allowed}");
}

#[test]
fn shell_exports_one_span_per_command_without_exporting_any_argument() {
    // The trace has to read as the ordered list of commands the script ran, and must carry none of
    // the argv those commands received. Asserting the absence is the point: a test that only
    // checked for the safe fields would pass just as happily beside a field leaking every one.
    let directory = tempfile::tempdir().expect("temporary directory");
    let trace = directory.path().join("trace.json");
    let provider = provider_path();
    let output = run(&[
        "--trace",
        trace.to_str().expect("UTF-8 trace path"),
        "shell",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "echo.upcase --message SENTINEL_DO_NOT_EXPORT | jq -r .message\nhelper_SENTINEL_DO_NOT_EXPORT() { echo inner; }\nhelper_SENTINEL_DO_NOT_EXPORT\nSENTINEL_DO_NOT_EXPORT_typo",
    ]);
    // 127, because the script deliberately ends on a word that resolves to nothing.
    assert_eq!(output.status.code(), Some(127), "{}", stderr(&output));

    let raw = std::fs::read_to_string(&trace).expect("trace file reads");
    let events: Vec<Value> = serde_json::from_str(&raw).expect("trace is valid JSON");
    // The Chrome layer renders field values through `Debug`, so a string arrives quoted.
    let text = |value: &Value| {
        value
            .as_str()
            .unwrap_or_default()
            .trim_matches('"')
            .to_owned()
    };
    let commands = events
        .iter()
        .filter(|event| event["name"] == "shell.command")
        .filter(|event| !event["args"]["outcome"].is_null())
        .map(|event| {
            let arguments = &event["args"];
            (
                text(&arguments["shell.command.kind"]),
                text(&arguments["shell.command.name"]),
                text(&arguments["outcome"]),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        commands,
        vec![
            (
                "capability".to_owned(),
                "echo.upcase".to_owned(),
                "succeeded".to_owned()
            ),
            (
                "builtin".to_owned(),
                "jq".to_owned(),
                "succeeded".to_owned()
            ),
            (
                "builtin".to_owned(),
                "echo".to_owned(),
                "succeeded".to_owned()
            ),
            (
                "function".to_owned(),
                "<withheld>".to_owned(),
                "succeeded".to_owned()
            ),
            (
                "not-found".to_owned(),
                "<withheld>".to_owned(),
                "not-found".to_owned()
            ),
        ]
    );

    // The argument value, the model-authored function name, and the unresolved word all appear in
    // the script; none of them may appear anywhere in the exported trace.
    assert!(
        !raw.contains("SENTINEL_DO_NOT_EXPORT"),
        "a script value reached the exported trace"
    );
}

#[test]
fn shell_rejects_a_malformed_curl_capability_at_parse_time() {
    // A raw `String` here turned a malformed identifier into a runtime "capability not found",
    // telling the operator the capability was missing when the value was simply not an identifier.
    let provider = provider_path();
    let output = run(&[
        "shell",
        "--provider",
        provider.to_str().expect("UTF-8 fixture path"),
        "--curl-capability",
        "not a valid id!!",
        "curl https://example.test/",
    ]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!stderr.contains("capability not found"), "{stderr}");
}

// ---------------------------------------------------------------------------
// Gateway chat client
// ---------------------------------------------------------------------------

/// What the stub gateway does with one request it received.
#[cfg(unix)]
enum GatewayReply {
    /// Answer with one well-formed `{"reply": ...}` line.
    Reply(&'static str),
    /// Answer with a line that is not a reply at all.
    Malformed(&'static str),
    /// Stop answering and close the connection.
    HangUp,
}

/// A `0700` directory to hold a `0600` socket, matching the hygiene the transport itself requires.
#[cfg(unix)]
fn gateway_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temporary gateway directory");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("secure gateway directory");
    directory
}

/// Stands up a stub of `dekopond`'s local transport: JSON lines in, `{"reply": ...}` lines out.
///
/// `chat` is a socket client, so its whole contract is exercisable with no gateway, broker, policy,
/// or model behind it. The handle returns every request line the stub actually received, which is
/// what makes the conversation identity assertable rather than merely plausible.
#[cfg(unix)]
fn stub_gateway(
    socket: &std::path::Path,
    script: Vec<GatewayReply>,
) -> thread::JoinHandle<Vec<Value>> {
    let listener = std::os::unix::net::UnixListener::bind(socket).expect("stub gateway binds");
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .expect("secure stub gateway socket");
    thread::spawn(move || {
        let (stream, _) = listener.accept().expect("stub gateway accepts");
        let mut requests =
            std::io::BufReader::new(stream.try_clone().expect("stub gateway clones its stream"));
        let mut replies = stream;
        let mut received = Vec::new();
        for reply in script {
            let mut line = String::new();
            if requests.read_line(&mut line).expect("stub gateway reads") == 0 {
                break;
            }
            received.push(serde_json::from_str(&line).expect("a request line is JSON"));
            match reply {
                GatewayReply::Reply(text) => {
                    writeln!(replies, "{}", json!({ "reply": text })).expect("stub gateway writes");
                }
                GatewayReply::Malformed(line) => {
                    writeln!(replies, "{line}").expect("stub gateway writes");
                }
                GatewayReply::HangUp => break,
            }
        }
        received
    })
}

/// Runs the binary with `input` on a piped standard input, which only `chat` needs.
#[cfg(unix)]
fn run_with_stdin(arguments: &[&str], input: &str) -> Output {
    let mut child = binary()
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("dekopon-run process starts");
    let mut stdin = child.stdin.take().expect("piped standard input");
    match stdin.write_all(input.as_bytes()) {
        Ok(()) => {}
        // A command that refuses a line stops reading and closes its end before we finish writing.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(error) => panic!("standard input writes: {error}"),
    }
    drop(stdin);
    child.wait_with_output().expect("dekopon-run process exits")
}

/// The property the whole feature rests on: one conversation identity, on every request.
#[cfg(unix)]
#[test]
fn chat_answers_each_message_and_carries_one_conversation_through_the_session() {
    let directory = gateway_directory();
    let socket = directory.path().join("dekopond-dev.sock");
    let gateway = stub_gateway(
        &socket,
        vec![
            GatewayReply::Reply("Nothing external."),
            GatewayReply::Reply("Two read-only capability calls."),
        ],
    );

    let output = run_with_stdin(
        &[
            "chat",
            "--gateway",
            socket.to_str().expect("UTF-8 socket path"),
            "--subject",
            "tel.16034700182",
            "--conversation",
            "morning-standup",
        ],
        // The blank line in the middle is deliberate: it asks nothing, so it is never sent.
        "what changed today?\n\nand what did it cost?\n",
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "Nothing external.\nTwo read-only capability calls.\n"
    );

    let requests = gateway.join().expect("stub gateway completes");
    assert_eq!(requests.len(), 2, "{requests:?}");
    for request in &requests {
        assert_eq!(request["subject"], "tel.16034700182");
        assert_eq!(request["channel"], "morning-standup");
    }
    assert_eq!(requests[0]["text"], "what changed today?");
    assert_eq!(requests[1]["text"], "and what did it cost?");
}

#[cfg(unix)]
#[test]
fn chat_ends_cleanly_when_input_ends() {
    let directory = gateway_directory();
    let socket = directory.path().join("dekopond-dev.sock");
    let gateway = stub_gateway(&socket, Vec::new());

    let output = run_with_stdin(
        &[
            "chat",
            "--gateway",
            socket.to_str().expect("UTF-8 socket path"),
            "--subject",
            "tel.16034700182",
            "--conversation",
            "empty-session",
        ],
        "",
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert!(gateway.join().expect("stub gateway completes").is_empty());
}

#[cfg(unix)]
#[test]
fn chat_reports_a_gateway_that_hangs_up_mid_session() {
    let directory = gateway_directory();
    let socket = directory.path().join("dekopond-dev.sock");
    let gateway = stub_gateway(
        &socket,
        vec![GatewayReply::Reply("still here"), GatewayReply::HangUp],
    );

    let output = run_with_stdin(
        &[
            "chat",
            "--gateway",
            socket.to_str().expect("UTF-8 socket path"),
            "--subject",
            "tel.16034700182",
            "--conversation",
            "interrupted",
        ],
        "are you there?\nstill?\n",
    );

    assert_eq!(output.status.code(), Some(1));
    // The answer that did arrive is kept; only the unanswered request fails the session.
    assert_eq!(stdout(&output), "still here\n");
    assert!(
        stderr(&output).contains("the gateway closed the connection"),
        "{}",
        stderr(&output)
    );
    assert_eq!(gateway.join().expect("stub gateway completes").len(), 2);
}

#[cfg(unix)]
#[test]
fn chat_reports_a_line_that_is_not_a_reply() {
    let directory = gateway_directory();
    let socket = directory.path().join("dekopond-dev.sock");
    let gateway = stub_gateway(&socket, vec![GatewayReply::Malformed("<html>oops</html>")]);

    let output = run_with_stdin(
        &[
            "chat",
            "--gateway",
            socket.to_str().expect("UTF-8 socket path"),
            "--subject",
            "tel.16034700182",
            "--conversation",
            "wrong-socket",
        ],
        "hello?\n",
    );

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("not a reply"),
        "{}",
        stderr(&output)
    );
    assert_eq!(gateway.join().expect("stub gateway completes").len(), 1);
}

#[cfg(unix)]
#[test]
fn chat_announces_a_minted_conversation_and_sends_exactly_that_value() {
    let directory = gateway_directory();
    let socket = directory.path().join("dekopond-dev.sock");
    let gateway = stub_gateway(&socket, vec![GatewayReply::Reply("noted")]);

    let output = run_with_stdin(
        &[
            "chat",
            "--gateway",
            socket.to_str().expect("UTF-8 socket path"),
            "--subject",
            "slack.t0123abc.u9xyz",
        ],
        "remember this\n",
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // The identifier goes to standard error, so a piped consumer reads replies and nothing else.
    assert_eq!(stdout(&output), "noted\n");
    let announced = stderr(&output)
        .strip_prefix("conversation: ")
        .expect("a minted conversation identifier is announced")
        .trim_end()
        .to_owned();
    assert!(announced.starts_with("chat-"), "{announced}");

    // Announcing an identifier the requests do not carry would make the session unresumable.
    let requests = gateway.join().expect("stub gateway completes");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["channel"], announced);
    assert_eq!(requests[0]["subject"], "slack.t0123abc.u9xyz");
}

#[cfg(unix)]
#[test]
fn chat_refuses_a_message_the_gateway_could_not_read() {
    // The transport's answer to an over-long line is to close the connection without a diagnostic,
    // so a client that sent one would report an unexplained hang-up instead of the real cause.
    let directory = gateway_directory();
    let socket = directory.path().join("dekopond-dev.sock");
    let gateway = stub_gateway(&socket, vec![GatewayReply::Reply("unreachable")]);

    let output = run_with_stdin(
        &[
            "chat",
            "--gateway",
            socket.to_str().expect("UTF-8 socket path"),
            "--subject",
            "tel.16034700182",
            "--conversation",
            "oversize",
        ],
        &format!("{}\n", "x".repeat(70 * 1024)),
    );

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).contains("65536"), "{}", stderr(&output));
    assert!(gateway.join().expect("stub gateway completes").is_empty());
}
