use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Output},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt as _, sync::Arc};

#[cfg(unix)]
use dekopon_broker::{Broker, BrokerLimits, InMemoryAuditLog, PolicyRule};
#[cfg(unix)]
use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry};
#[cfg(unix)]
use dekopon_broker_protocol::FrameLimits;
#[cfg(unix)]
use dekopon_brokerd::{BrokerServer, ServerLimits, current_uid};
#[cfg(unix)]
use dekopon_capability::{EffectKind, ExecutionConstraints, Idempotency};
#[cfg(unix)]
use dekopon_core::{Actor, AgentId, PrincipalId, RiskLevel};
use serde_json::{Value, json};
#[cfg(unix)]
use tokio::{net::UnixListener, sync::oneshot};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dekopon-run"))
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
fn direct_mode_rejects_the_http_importing_provider() {
    for fixture in ["http-probe-provider.wasm", "jsonplaceholder-provider.wasm"] {
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
            vec![PolicyRule {
                principal: principal.clone(),
                actor: actor.clone(),
                capability: "echo.echo".parse().expect("valid capability fixture"),
                provider: "echo".parse().expect("valid provider fixture"),
                effect: EffectKind::ReadOnly,
                risk: RiskLevel::Low,
                idempotency: Idempotency::Idempotent,
                constraints: ExecutionConstraints::default(),
            }],
            Arc::clone(&audit),
            BrokerLimits::default(),
        )
        .expect("build broker fixture"),
    );
    let mut identities = BTreeMap::new();
    identities.insert(
        uid,
        dekopon_broker::AuthenticatedContext::new(principal, actor).expect("bind fixture context"),
    );
    let limits = ServerLimits {
        frame: FrameLimits::default(),
        max_connections: 4,
        shutdown_grace: Duration::from_secs(2),
    };
    let server = BrokerServer::new(broker, identities, limits).expect("build server fixture");
    let (shutdown_send, shutdown_receive) = oneshot::channel::<()>();
    let server_task = tokio::spawn(server.serve(listener, async move {
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
    assert!(names.contains(&"runner.invoke"));
    assert!(names.contains(&"provider.compile"));
    assert!(names.contains(&"provider.invoke"));
}

#[test]
fn runs_an_openai_compatible_prompt_tool_loop() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("mock endpoint binds");
    let address = listener.local_addr().expect("mock endpoint address");
    let server = thread::spawn(move || {
        let (first, first_stream) = read_request(&listener);
        assert_eq!(first["model"], "test-model");
        let tools = first["tools"].as_array().expect("tools are an array");
        assert_eq!(tools.len(), 5);
        assert!(tools.iter().any(|tool| {
            tool["function"]["name"] == "echo_echo"
                && tool["function"]["parameters"]["additionalProperties"] == true
        }));
        assert!(tools.iter().any(|tool| {
            tool["function"]["name"] == "echo_ransom_case"
                && tool["function"]["parameters"]["required"] == json!(["message"])
        }));
        respond(
            first_stream,
            &json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "echo_echo",
                                "arguments": "{\"message\":\"from model\"}"
                            }
                        }]
                    }
                }]
            }),
        );

        let (second, second_stream) = read_request(&listener);
        let tool_message = second["messages"]
            .as_array()
            .expect("messages are an array")
            .iter()
            .find(|message| message["role"] == "tool")
            .expect("tool result is returned to model");
        assert_eq!(tool_message["tool_call_id"], "call-1");
        assert_eq!(tool_message["content"], r#"{"message":"from model"}"#);
        respond(
            second_stream,
            &json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "The echo provider returned: from model.",
                        "tool_calls": []
                    }
                }]
            }),
        );
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
        "Use the echo tool",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8(output.stdout).expect("stdout UTF-8"),
        "The echo provider returned: from model.\n"
    );
    server.join().expect("mock endpoint completes");
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

// ---------------------------------------------------------------------------
// Sandboxed shell subcommand
// ---------------------------------------------------------------------------

/// Runs one script against the checked-in echo provider, returning stdout and the exit code.
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
fn shell_rejects_every_dropped_grammar_feature_rather_than_ignoring_it() {
    // An unquoted `*` is an ordinary character: there is no filesystem to glob against.
    let (stdout, code) = shell("echo *", &[]);
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(stdout, "*\n[exit code: 0]\n");

    // A trailing `&` is a hard parse error, never a silently dropped backgrounding request.
    let (stdout, code) = shell("echo hi &", &[]);
    assert_eq!(code, 2, "{stdout}");
    assert!(stdout.contains("backgrounding"), "{stdout}");
    assert!(!stdout.contains("hi\n"), "{stdout}");

    // `eval` is excluded as a sandbox-escape-shaped feature.
    let (stdout, code) = shell("eval 'echo pwned'", &[]);
    assert_eq!(code, 2, "{stdout}");
    assert!(stdout.contains("eval"), "{stdout}");
    assert!(!stdout.contains("pwned"), "{stdout}");
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
