#![allow(clippy::unwrap_used, clippy::disallowed_methods)]

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use dekopon_capability::InvocationOutcome;
use dekopon_core::{Redacted, VM_SESSION_HEADER};
use dekopon_model_proxy::{Grant, Granted, ModelProxy, ProxyModel, SUBJECT_HEADER, Upstream};
use dekopon_model_token_governor::{Metering, Tokens};
use dekopon_test_support::LoopbackServer;
use opentelemetry_sdk::logs::{LogBatch, LogExporter, SdkLoggerProvider};
use parking_lot::Mutex;

#[path = "common/http_probe.rs"]
mod http_probe;

use http_probe::{http_probe_broker, invoke_as_caller, request};

const SESSION: &str = "019a3c2e-7b4d-7c1a-9e2f-0123456789ab";
const BROKERD_LOG_FILTER: &str = "dekopon_brokerd=trace,dekopon_broker=trace,dekopon_broker_host=trace,dekopon_http_host=trace,gateway=debug,prompt=debug,model=debug,asset=debug,shell=debug,job=debug,broker=debug,provider=debug,http=debug,credential=debug,memory=debug,telemetry=debug,hyper=off,h2=off,reqwest=off";
const GATEWAYD_LOG_FILTER: &str = "job=debug,meter=info";

type Attributes = BTreeMap<String, String>;

#[derive(Clone, Debug, Default)]
struct ExportedLogs(Arc<Mutex<Vec<Attributes>>>);

impl LogExporter for ExportedLogs {
    async fn export(&self, batch: LogBatch<'_>) -> opentelemetry_sdk::error::OTelSdkResult {
        for (record, _) in batch.iter() {
            let attributes = record
                .attributes_iter()
                .filter_map(|(key, value)| {
                    let opentelemetry::logs::AnyValue::String(text) = value else {
                        return None;
                    };
                    Some((key.as_str().to_owned(), text.as_str().to_owned()))
                })
                .collect();
            self.0.lock().push(attributes);
        }
        Ok(())
    }
}

impl ExportedLogs {
    fn only(&self, name: &str, value: &str) -> Attributes {
        let records = self.0.lock();
        let matching = records
            .iter()
            .filter(|record| record.get(name).map(String::as_str) == Some(value))
            .collect::<Vec<_>>();
        let [record] = matching[..] else {
            panic!("one exported record with {name}={value}: {records:?}");
        };
        record.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vm_meter_record_and_its_launching_execution_export_the_same_session() {
    let exported = ExportedLogs::default();
    let telemetry = dekopon_telemetry::Install::new(dekopon_telemetry::Console {
        format: dekopon_telemetry::ConsoleFormat::Json,
        writer: dekopon_telemetry::ConsoleWriter::Stderr,
        filter: dekopon_telemetry::ConsoleFilter::Directive("off".into()),
    })
    .with_logs(
        SdkLoggerProvider::builder()
            .with_simple_exporter(exported.clone())
            .build(),
        format!("{BROKERD_LOG_FILTER},{GATEWAYD_LOG_FILTER}"),
    )
    .install()
    .unwrap();

    let runner = LoopbackServer::once(
        format!("HTTP/1.1 201 Created\r\nContent-Type: application/json\r\n{VM_SESSION_HEADER}: {SESSION}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}")
            .as_bytes(),
    );
    let authority = runner.authority().to_owned();
    let broker = http_probe_broker(&authority).await;
    let launched = invoke_as_caller(
        &broker,
        request(
            "vm-launch",
            "http-probe.fetch",
            serde_json::json!({"uri": format!("http://{authority}/v1/sessions"), "method": "GET"}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(launched.result.outcome, InvocationOutcome::Succeeded);
    runner.join();

    let answer = r#"{"id":"x","usage":{"prompt_tokens":5,"completion_tokens":2}}"#;
    let model = LoopbackServer::once(
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len())
            .as_bytes(),
    );
    let proxy = Arc::new(
        ModelProxy::new(
            vec![ProxyModel {
                name: "glm-flash".to_owned(),
                wire_model: "z-ai/glm-4.5-air".to_owned(),
                backend: "openrouter",
                reserve: Tokens(10),
                upstream: Upstream::OpenRouter {
                    endpoint: format!("http://{}", model.authority()),
                    api_key: Redacted::new("sk-or-proxy".to_owned()),
                    settings: Vec::new(),
                },
            }],
            HashMap::from([(
                "dekopon:gylmar-vm".to_owned(),
                Grant {
                    agent: "gylmar".parse().unwrap(),
                    models: Granted::Listed(BTreeSet::from(["glm-flash".to_owned()])),
                },
            )]),
            Arc::new(Metering::new(Vec::new(), Metering::system_clock())),
        )
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let serving = tokio::spawn(std::future::IntoFuture::into_future(axum::serve(
        listener,
        proxy.router(),
    )));
    let response = reqwest::Client::new()
        .post(url)
        .header(SUBJECT_HEADER, "dekopon:gylmar-vm")
        .header(VM_SESSION_HEADER, SESSION)
        .header("content-type", "application/json")
        .body(r#"{"model":"glm-flash","messages":[],"stream":false}"#)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), answer);
    assert!(!model.request_text().contains(SESSION));
    model.join();
    serving.abort();
    telemetry.shutdown().unwrap();

    let execution = exported.only("audit.event", "broker.execution");
    let meter = exported.only("meter.via", "proxy");
    assert_eq!(execution["invocation.id"], "vm-launch");
    assert_eq!(execution["vm.session"], SESSION);
    assert_eq!(meter["vm.session"], execution["vm.session"]);
}
