use super::*;
use dekopon_model::inference::ModelClient;
use dekopon_model_token_governor::{Metering, UnixMillis};
use dekopon_test_support::LoopbackServer;
use std::sync::atomic::AtomicI64;

const BEFORE_MIDNIGHT: i64 = 1_793_491_199_987;
const AFTER_MIDNIGHT: i64 = 1_793_491_200_123;
const OCTOBER: &str = "2026-10-31T23:59:59Z";
const NOVEMBER: &str = "2026-11-01T00:00:00Z";
const REFERENCE: &str = "[gateway: Dekopon runtime time reference, sampled for this task: ";

pub(super) fn payload_message(message: &ModelMessage) -> (String, String) {
    let (role, text) = message_text(message);
    let payload = if role == "user" {
        text.rsplit_once(&format!("\n\n{REFERENCE}"))
            .filter(|(_, sample)| sample.ends_with("otherwise state the sample's age limitation.]"))
            .map_or(text.as_str(), |(payload, _)| payload)
    } else {
        &text
    };
    (role, payload.to_owned())
}

fn clocked(runner: Arc<SessionRunner>, clock: &Arc<AtomicI64>) -> Arc<SessionRunner> {
    let mut runner = Arc::into_inner(runner).expect("a fresh runner has one owner");
    let clock = Arc::clone(clock);
    runner.metering = Arc::new(Metering::new(
        Vec::new(),
        Arc::new(move || UnixMillis(clock.load(Ordering::SeqCst))),
    ));
    Arc::new(runner)
}

fn reference(text: &str) -> &str {
    text.split_once(REFERENCE)
        .expect("runtime-produced reference")
        .1
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_task_keeps_its_sample_and_the_next_task_refreshes_across_utc_midnight() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = InterruptibleModel::new([
        script_call("printf working"),
        answer("done"),
        answer("next"),
    ]);
    let clock = Arc::new(AtomicI64::new(BEFORE_MIDNIGHT));
    let runner = clocked(
        runner_with(broker, Arc::new(Arc::clone(&models)), 1),
        &clock,
    );
    let route = persistent_route(model_config(), window());
    let driver = Arc::new(RecordingDriver::default());
    let running = tokio::spawn(run_session(
        Arc::clone(&runner),
        route.clone(),
        message("first"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    models.wait_until_entered().await;
    clock.store(AFTER_MIDNIGHT, Ordering::SeqCst);
    models.release();
    running.await.unwrap();
    run_session(
        runner,
        route,
        message("second"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    let prompts = models.prompts.lock();
    assert_eq!(prompts.len(), 3);
    let first = prompts[0].last().unwrap().content().unwrap();
    let sample = reference(first);
    assert_eq!(sample.split_once(" (UTC).").unwrap().0, OCTOBER);
    assert!(sample.ends_with("otherwise state the sample's age limitation.]"));
    assert!(sample.contains("fixed sample, not a continuously advancing clock"));
    assert!(sample.contains("UTC does not establish your timezone"));
    assert_eq!(&prompts[1][..prompts[0].len()], &prompts[0]);
    assert_eq!(
        reference(prompts[2].last().unwrap().content().unwrap())
            .split_once(" (UTC).")
            .unwrap()
            .0,
        NOVEMBER
    );
    assert_eq!(prompts[0][0], prompts[2][0]);
    assert_eq!(
        prompts[2][2].content(),
        Some("[gateway: unmapped participant]\nfirst")
    );
    assert_eq!(driver.replies(), ["done", "next"]);
}

struct WireModels(Arc<ModelClient>);

impl ModelFactory for WireModels {
    fn build_bound(
        &self,
        model: &Arc<ModelConfig>,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        Ok(Arc::new(dekopon_model::blocking::BlockingModel::new(
            Arc::clone(&self.0),
            runtime,
            cancel,
            Duration::from_millis(model.timeout_ms().unwrap()),
        )))
    }
}

#[derive(Clone, Copy, Debug)]
enum Adapter {
    Codex,
    Chatgpt,
    Openai,
    Openrouter,
}

impl Adapter {
    fn response(self) -> Vec<u8> {
        let body = match self {
            Self::Codex | Self::Chatgpt => concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\"}}\n\n"
            ),
            Self::Openai | Self::Openrouter => concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            ),
        };
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
    }

    fn client(self, url: &str, auth: &Path) -> ModelClient {
        use dekopon_model::{
            codex::CodexClient,
            openai::OpenAiClient,
            openrouter::{
                OpenRouterClient,
                settings::{Cache, CacheStyle, Settings},
            },
        };
        let timeout = Duration::from_secs(5);
        match self {
            Self::Codex => ModelClient::Codex(
                CodexClient::new("gpt-6-luna", Some(auth), timeout, &[])
                    .unwrap()
                    .with_loopback_endpoint(&format!("{url}/codex/responses"))
                    .unwrap(),
            ),
            Self::Chatgpt => {
                let config = ModelConfig::ChatgptSubscription {
                    ca_bundles: Default::default(),
                    name: "luna-long".into(),
                    model: "gpt-6-luna".into(),
                    auth_file: Some(auth.to_owned()),
                    timeout_ms: 900_000,
                    classes: vec!["reasoning".into()],
                    modalities: Vec::new(),
                };
                let client = crate::session::ConfiguredModels::default()
                    .construct_with(&config, |_| None)
                    .unwrap();
                let ModelClient::Codex(client) = Arc::try_unwrap(client).ok().unwrap() else {
                    panic!("subscription uses Codex Responses")
                };
                ModelClient::Codex(
                    client
                        .with_loopback_endpoint(&format!("{url}/chatgpt/responses"))
                        .unwrap(),
                )
            }
            Self::Openai => ModelClient::OpenAiCompatible(
                OpenAiClient::new(format!("{url}/openai/v1"), "gpt-test", None, timeout, &[])
                    .unwrap(),
            ),
            Self::Openrouter => ModelClient::OpenRouter(
                OpenRouterClient::new(
                    "anthropic/test",
                    "synthetic-key".into(),
                    timeout,
                    Settings {
                        cache: Some(Cache {
                            style: CacheStyle::ExplicitPrefix,
                            ttl: None,
                        }),
                        ..Settings::default()
                    },
                    &[],
                )
                .unwrap()
                .with_loopback_endpoint(&format!("{url}/router/chat/completions"))
                .unwrap(),
            ),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gateway_task_time_reaches_each_real_adapter_after_the_stable_prefix() {
    for adapter in [
        Adapter::Codex,
        Adapter::Chatgpt,
        Adapter::Openai,
        Adapter::Openrouter,
    ] {
        let server = LoopbackServer::serving(&adapter.response(), 2);
        let directory = temporary();
        let auth = directory.path().join("synthetic-auth.json");
        fs::write(
            &auth,
            json!({"version":1,"access":"synthetic-access","refresh":"synthetic-refresh",
            "expiresAt":4_000_000_000_u64,"accountId":"synthetic-account"})
            .to_string(),
        )
        .unwrap();
        let client = adapter.client(&server.url(), &auth);
        let (broker, _observed) =
            stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
        let clock = Arc::new(AtomicI64::new(BEFORE_MIDNIGHT));
        let runner = clocked(
            runner_with(broker, Arc::new(WireModels(Arc::new(client))), 1),
            &clock,
        );
        let mut route = persistent_route(model_config(), window());
        route.inspect_agent_config = false;
        let driver = Arc::new(RecordingDriver::default());
        run_session(
            Arc::clone(&runner),
            route.clone(),
            message("first"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
        clock.store(AFTER_MIDNIGHT, Ordering::SeqCst);
        run_session(
            runner,
            route,
            message("second"),
            Arc::clone(&driver) as Arc<dyn ChatDriver>,
        )
        .await;
        assert_eq!(driver.replies(), ["done", "done"], "{adapter:?}");
        let requests = server.recorded();
        assert_eq!(requests.len(), 2, "{adapter:?}");
        let bodies: Vec<Value> = requests
            .iter()
            .map(|request| {
                let text = std::str::from_utf8(request).unwrap();
                let (head, body) = text.split_once("\r\n\r\n").unwrap();
                let path = match adapter {
                    Adapter::Codex => "/codex/responses",
                    Adapter::Chatgpt => "/chatgpt/responses",
                    Adapter::Openai => "/openai/v1/chat/completions",
                    Adapter::Openrouter => "/router/chat/completions",
                };
                assert_eq!(
                    head.lines().next(),
                    Some(format!("POST {path} HTTP/1.1").as_str())
                );
                assert!(!body.contains("synthetic-access"));
                assert!(!body.contains("synthetic-refresh"));
                serde_json::from_str(body).unwrap()
            })
            .collect();
        assert_eq!(bodies[0]["tools"], bodies[1]["tools"]);
        for (index, body) in bodies.iter().enumerate() {
            let expected = [OCTOBER, NOVEMBER][index];
            match adapter {
                Adapter::Codex | Adapter::Chatgpt => {
                    assert!(!body["instructions"].as_str().unwrap().contains(REFERENCE));
                    assert!(
                        body["instructions"]
                            .as_str()
                            .unwrap()
                            .starts_with("Answer briefly.")
                    );
                    let input = body["input"].as_array().unwrap();
                    let text = input.last().unwrap()["content"][0]["text"]
                        .as_str()
                        .unwrap();
                    assert_eq!(reference(text).split_once(" (UTC).").unwrap().0, expected);
                    if index == 1 {
                        assert_eq!(
                            input[0]["content"][0]["text"],
                            "[gateway: unmapped participant]\nfirst"
                        );
                    }
                }
                Adapter::Openai | Adapter::Openrouter => {
                    let messages = body["messages"].as_array().unwrap();
                    assert_eq!(messages[0], bodies[0]["messages"][0]);
                    let content = &messages.last().unwrap()["content"];
                    let text = content
                        .as_str()
                        .unwrap_or_else(|| content[0]["text"].as_str().unwrap());
                    assert_eq!(reference(text).split_once(" (UTC).").unwrap().0, expected);
                    if index == 1 {
                        let prior = &messages[2]["content"];
                        assert_eq!(
                            prior
                                .as_str()
                                .unwrap_or_else(|| prior[0]["text"].as_str().unwrap()),
                            "[gateway: unmapped participant]\nfirst"
                        );
                    }
                    if let Adapter::Openrouter = adapter {
                        assert_eq!(
                            messages[0]["content"][0]["cache_control"]["type"],
                            "ephemeral"
                        );
                        assert_eq!(
                            messages.last().unwrap()["content"][0]["cache_control"]["type"],
                            "ephemeral"
                        );
                        if index == 1 {
                            assert_eq!(
                                messages[3]["content"][0]["cache_control"]["type"],
                                "ephemeral"
                            );
                        }
                    }
                }
            }
        }
        fs::remove_file(auth).unwrap();
        server.join();
        eprintln!(
            "{adapter:?}: 2 gateway requests captured; UTC reference in newest user context after stable instructions/history; tools unchanged"
        );
    }
}
