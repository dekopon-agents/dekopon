use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Command, Output},
    thread,
    time::Duration,
};

use serde_json::{Value, json};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dekopon-run"))
}

fn provider_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/providers/echo-provider.wasm")
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
