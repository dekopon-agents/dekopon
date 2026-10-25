//! Real Unix broker + pure Wasm provider + loopback model/OTLP. No external requests or credentials.
use super::*;
use dekopon_agent::prompt::History;
use dekopon_model::{blocking::BlockingModel, inference::ModelClient, openai::OpenAiClient};
use dekopon_telemetry::{
    Console, ConsoleFilter, ConsoleFormat, ConsoleWriter, ExporterSettings, Install, Transport,
};
use opentelemetry_proto::tonic::{
    collector::{logs::v1::ExportLogsServiceRequest, trace::v1::ExportTraceServiceRequest},
    trace::v1::Span,
};
use prost::Message as _;
use std::{
    io::{BufRead as _, Read as _, Write as _},
    net::TcpListener,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
use tracing::Instrument as _;

#[derive(Default)]
struct Captured {
    spans: Vec<Span>,
    logs: Vec<opentelemetry_proto::tonic::logs::v1::LogRecord>,
    model_requests: Vec<serde_json::Value>,
}
struct Loopback {
    endpoint: String,
    data: Arc<Mutex<Captured>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Loopback {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let data = Arc::new(Mutex::new(Captured::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&data);
        let stopping = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("loopback accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(&mut stream);
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap().to_owned();
                let mut length = None;
                let mut model_auth = false;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = Some(value.trim().parse::<usize>().unwrap());
                        }
                        if name.eq_ignore_ascii_case("authorization") {
                            model_auth = value.trim() == "Bearer synthetic-model-credential";
                        }
                        assert!(
                            !name.eq_ignore_ascii_case("traceparent"),
                            "do not send internal trace to model endpoint"
                        );
                    }
                }
                let mut body = vec![0; length.expect("bounded fixture content-length")];
                reader.read_exact(&mut body).unwrap();
                let mut data = captured.lock().unwrap();
                let response = match path.as_str() {
                    "/v1/traces" => {
                        assert!(
                            !String::from_utf8_lossy(&body).contains("synthetic-model-credential")
                        );
                        for resource in ExportTraceServiceRequest::decode(body.as_slice())
                            .unwrap()
                            .resource_spans
                        {
                            for scope in resource.scope_spans {
                                data.spans.extend(scope.spans);
                            }
                        }
                        Vec::new()
                    }
                    "/v1/logs" => {
                        assert!(
                            !String::from_utf8_lossy(&body).contains("synthetic-model-credential")
                        );
                        for resource in ExportLogsServiceRequest::decode(body.as_slice())
                            .unwrap()
                            .resource_logs
                        {
                            for scope in resource.scope_logs {
                                data.logs.extend(scope.log_records);
                            }
                        }
                        Vec::new()
                    }
                    "/chat/completions" => {
                        assert!(
                            model_auth,
                            "synthetic credential actually exercises the model client"
                        );
                        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        let first = data.model_requests.len() % 2 == 0;
                        data.model_requests.push(request);
                        let message = if first {
                            json!({"role":"assistant", "content":null, "tool_calls":[{"id":"synthetic-call", "type":"function", "function":{"name":"bash", "arguments":json!({"command":"probe upper --text synthetic-payload"}).to_string()}}]})
                        } else {
                            json!({"role":"assistant", "content":"synthetic-answer"})
                        };
                        serde_json::to_vec(&json!({"choices":[{"message":message}]})).unwrap()
                    }
                    _ => panic!("unexpected loopback path {path}"),
                };
                drop(data);
                let content_type = if path.ends_with("completions") {
                    "application/json"
                } else {
                    "application/x-protobuf"
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).unwrap();
                stream.write_all(&response).unwrap();
            }
        });
        Self {
            endpoint,
            data,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Err(error) = self.thread.take().unwrap().join()
            && !thread::panicking()
        {
            std::panic::resume_unwind(error);
        }
    }
}

fn is_descendant(spans: &[Span], child: &Span, ancestor: &Span) -> bool {
    let mut parent = child.parent_span_id.clone();
    for _ in 0..spans.len() {
        if parent == ancestor.span_id {
            return true;
        }
        let Some(span) = spans
            .iter()
            .find(|s| s.span_id == parent && s.trace_id == child.trace_id)
        else {
            return false;
        };
        parent = span.parent_span_id.clone();
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_turn_exports_one_causal_trace_through_model_script_broker_and_provider() {
    if std::env::var_os("DEKOPON_TEST_BROKERD").is_none()
        || std::env::var_os("DEKOPON_TEST_PROBE_WASM").is_none()
    {
        eprintln!("broker fixture env absent; trace integration not exercised");
        return;
    }
    let collector = Loopback::start();
    let mut fixture = broker_with_telemetry(true, false, false, Some(&collector.endpoint))
        .await
        .unwrap();
    let settings = ExporterSettings::new(
        &collector.endpoint,
        Transport::Http,
        "console-trace-fixture",
        "dekopon-console",
        "fixture",
        Duration::from_secs(5),
    )
    .unwrap();
    let filter = "off,dekopon_tui=trace,dekopon_agent=trace,dekopon_model=trace,dekopon_process=trace,dekopon_shell=trace";
    let telemetry = Install::new(Console {
        format: ConsoleFormat::Text {
            ansi: Some(false),
            target: false,
            timestamps: false,
        },
        writer: ConsoleWriter::Stderr,
        filter: ConsoleFilter::Directive("off".into()),
    })
    .with_traces(
        settings.tracer_provider().unwrap(),
        "console-fixture",
        filter,
    )
    .with_logs(settings.logger_provider().unwrap(), filter)
    .with_shutdown_timeout(Duration::from_secs(5))
    .install()
    .unwrap();
    let mut options = ConsoleOptions::new(
        "slack.t0123abc.u9xyz".parse().unwrap(),
        "synthetic-model".into(),
    );
    options.socket = Some(fixture.socket.clone());
    let (client, _) = connect(&options).await.unwrap();
    let mut traces = Vec::new();
    for _ in 0..2 {
        let span = dekopon_tui::run::console_turn_span("model");
        let trace = async {
            // The same async opening boundary as dispatch; the leg must be born inside the turn.
            let leg = open_agent(client.clone(), options.subject.clone(), "reviewer".parse().unwrap(), None).await.unwrap();
            let trace = leg.session_trace().to_string();
            let client = OpenAiClient::new(&collector.endpoint, "synthetic-model", Some("synthetic-model-credential".into()), Duration::from_secs(10)).unwrap().with_streaming(false);
            let model = BlockingModel::new(Arc::new(ModelClient::OpenAiCompatible(client)), tokio::runtime::Handle::current(), tokio::sync::watch::channel(false).1, Duration::from_secs(10));
            let agent = serde_json::from_value(json!({"apiVersion":"dekopon.dev/v1alpha1", "kind":"Agent", "metadata":{"name":"reviewer"}, "spec":{"description":"synthetic agent", "enabled":true}})).unwrap();
            let (events, mut receiver) = dekopon_tui::session::session_channel();
            let progress = Arc::new(dekopon_tui::record::RecordingProgress::new(events.clone()));
            let history = tokio::spawn(dekopon_tui::session::run_turn(leg, Arc::new(model), "synthetic-prompt".into(), Some("synthetic-instructions".into()), agent, options.clone(), History::new(options.history_limits), Default::default(), progress, events).in_current_span()).await.unwrap().unwrap();
            assert_eq!(history.len(), 1);
            let mut finished = false;
            while let Some(event) = receiver.recv().await {
                if let SessionEvent::Finished(outcome) = event {
                    let outcome = outcome.unwrap();
                    assert_eq!(outcome.answer, "synthetic-answer");
                    assert_eq!(outcome.capability_invocations, 1);
                    finished = true;
                }
            }
            assert!(finished);
            trace
        }.instrument(span).await;
        traces.push(trace);
    }
    assert_ne!(traces[0], traces[1], "one trace per turn, not per agent");
    assert!(
        Command::new("kill")
            .args(["-TERM", &fixture.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        fixture.child.wait().unwrap().success(),
        "broker orderly shutdown flushes OTLP"
    );
    telemetry.shutdown().unwrap();
    let data = collector.data.lock().unwrap();
    assert_eq!(data.model_requests.len(), 4);
    assert!(
        data.model_requests[1]
            .to_string()
            .contains("SYNTHETIC-PAYLOAD"),
        "real provider result returns to the model"
    );
    for trace in traces {
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let spans: Vec<_> = data
            .spans
            .iter()
            .filter(|s| hex(&s.trace_id) == trace)
            .cloned()
            .collect();
        let find = |name: &str| {
            spans
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("missing {name}: {spans:?}"))
        };
        let root = find("console.turn");
        assert!(root.parent_span_id.is_empty());
        let session = find("prompt.session");
        assert_eq!(session.parent_span_id, root.span_id);
        assert_eq!(find("prompt.model_turn").parent_span_id, session.span_id);
        let script = find("prompt.script");
        assert!(is_descendant(&spans, script, session));
        for name in [
            "shell.command",
            "broker.command_run",
            "broker.invocation",
            "provider.invoke",
        ] {
            assert!(
                is_descendant(&spans, find(name), script),
                "{name} must descend from the script, not merely share a trace"
            );
        }
        assert!(is_descendant(
            &spans,
            find("provider.invoke"),
            find("broker.invocation")
        ));
        let logs: Vec<_> = data
            .logs
            .iter()
            .filter(|l| hex(&l.trace_id) == trace)
            .collect();
        let rendered = format!("{logs:?}");
        for payload in [
            "synthetic-prompt",
            "synthetic-instructions",
            "synthetic-answer",
            "probe upper",
            "SYNTHETIC-PAYLOAD",
            "dekopon-console",
        ] {
            assert!(
                rendered.contains(payload),
                "complete correlated payload/origin missing {payload}: {rendered}"
            );
        }
        eprintln!(
            "verified turn trace {trace}: {} spans, {} correlated logs; causal model/script/broker/provider chain and full synthetic payloads",
            spans.len(),
            logs.len()
        );
        assert!(
            logs.iter()
                .all(|l| spans.iter().any(|s| s.span_id == l.span_id)),
            "logs reference exported spans"
        );
    }
}
