#![allow(clippy::unwrap_used)]

use dekopon_agent::prompt::{
    AssetSource, FetchedAsset, History, PromptLimits, ScriptRuntime, SessionInputs,
    run_prompt_session,
};
use dekopon_model::{
    asset::DiskBlob,
    blocking::BlockingModel,
    codex::CodexClient,
    inference::ModelClient,
    openrouter::{OpenRouterClient, settings::Settings},
};
use dekopon_shell::{ExitCode, ScriptOutcome};
use dekopon_test_support::LoopbackServer;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
struct SyntheticRuntime(Mutex<Vec<String>>);
impl ScriptRuntime for SyntheticRuntime {
    fn capability_calls_used(&self) -> u32 {
        0
    }

    fn run_script(&self, script: &str) -> ScriptOutcome {
        self.0.lock().push(script.to_owned());
        ScriptOutcome {
            output: "synthetic-result".into(),
            exit_code: ExitCode::SUCCESS,
            truncated: false,
            capability_calls: 0,
            steps: 0,
        }
    }
}

fn response(body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}
fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[derive(Clone, Copy)]
enum Dialect {
    Codex,
    OpenRouter,
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_prompt_loop_executes_one_synthetic_tool_and_replays_each_native_dialect() {
    for dialect in [Dialect::Codex, Dialect::OpenRouter] {
        let (tool, answer) = match dialect {
            Dialect::Codex => (
                include_str!("../../dekopon-model/src/fixtures/codex-script.sse"),
                include_str!("../../dekopon-model/src/fixtures/codex-answer.sse"),
            ),
            Dialect::OpenRouter => (
                include_str!("../../dekopon-model/src/fixtures/openrouter-script.sse"),
                include_str!("../../dekopon-model/src/fixtures/openrouter-answer.sse"),
            ),
        };
        let server = LoopbackServer::sequence([response(tool), response(answer)]);
        let directory = tempfile::tempdir().unwrap();
        let credential = directory.path().join("auth.json");
        std::fs::write(
            &credential,
            serde_json::to_vec(&json!({
                "version":1,"access":"fake-access","refresh":"fake-refresh",
                "expiresAt": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()+3600,
                "accountId":"fake-account"
            }))
            .unwrap(),
        )
        .unwrap();
        let timeout = Duration::from_secs(5);
        let client = match dialect {
            Dialect::Codex => ModelClient::Codex(
                CodexClient::new("gpt-test", Some(&credential), timeout)
                    .unwrap()
                    .with_loopback_endpoint(&server.url())
                    .unwrap(),
            ),
            Dialect::OpenRouter => ModelClient::OpenRouter(
                OpenRouterClient::new(
                    "vendor/model",
                    "fake-key".into(),
                    timeout,
                    Settings::default(),
                )
                .unwrap()
                .with_loopback_endpoint(&server.url())
                .unwrap(),
            ),
        };
        let (_sender, receiver) = tokio::sync::watch::channel(false);
        let bridge = BlockingModel::new(
            Arc::new(client),
            tokio::runtime::Handle::current(),
            receiver,
            timeout,
        );
        let (outcome, scripts) = tokio::task::spawn_blocking(move || {
            let runtime = SyntheticRuntime::default();
            let outcome = run_prompt_session(
                &bridge,
                &runtime,
                SessionInputs::new(
                    "Run the synthetic tool once.",
                    PromptLimits {
                        max_steps: 3,
                        max_capability_calls: 1,
                    },
                )
                .with_system(Some("Synthetic offline proof.")),
                &mut History::default(),
            )
            .unwrap();
            (outcome, runtime.0.into_inner())
        })
        .await
        .unwrap();
        assert_eq!(outcome.model_turns, 2);
        assert_eq!(outcome.script_calls, 1);
        assert_eq!(outcome.capability_invocations, 0);
        assert_eq!(scripts, ["printf safe"]);
        let first = server.request_text();
        let second = server.request_text();
        for request in [&first, &second] {
            assert!(!request.to_ascii_lowercase().contains("traceparent:"));
            assert!(request.to_ascii_lowercase().contains("content-length:"));
            if matches!(dialect, Dialect::OpenRouter) {
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("x-openrouter-cache: false")
                );
            }
        }
        let first = body(&first);
        let replay = body(&second);
        assert_eq!(first["tools"], replay["tools"]);
        match dialect {
            Dialect::Codex => {
                assert_eq!(outcome.answer, "Echoed hello.");
                assert_eq!(first["instructions"], replay["instructions"]);
                assert_eq!(first["input"][0], replay["input"][0]);
                assert_eq!(replay["input"][1]["encrypted_content"], "opaque");
                assert_eq!(replay["input"][1]["future_field"], "preserved");
                assert_eq!(replay["input"][2]["call_id"], "call_1");
                assert_eq!(replay["input"][3]["type"], "function_call_output");
                assert!(
                    replay["input"][3]["output"]
                        .as_str()
                        .unwrap()
                        .contains("synthetic-result")
                );
            }
            Dialect::OpenRouter => {
                assert_eq!(outcome.answer, "done");
                assert_eq!(first["messages"][0], replay["messages"][0]);
                assert_eq!(
                    replay["messages"][2]["reasoning_details"][0]["text"],
                    "think-more"
                );
                assert_eq!(
                    replay["messages"][2]["reasoning_details"][1]["data"],
                    "cipher-tail"
                );
                assert_eq!(
                    replay["messages"][2]["tool_calls"][0]["function"]["name"],
                    "bash"
                );
                assert_eq!(replay["messages"][3]["tool_call_id"], "call-1");
                assert!(
                    replay["messages"][3]["content"]
                        .as_str()
                        .unwrap()
                        .contains("synthetic-result")
                );
            }
        }
        server.join();
    }
}

fn summarize_wire_images(messages: &mut Value) -> usize {
    use base64::Engine as _;
    use sha2::{Digest as _, Sha256};
    let mut images = 0;
    for message in messages.as_array_mut().unwrap() {
        if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
            for part in parts {
                *part = match part["type"].as_str().unwrap() {
                    "text" => part["text"].clone(),
                    "image_url" => {
                        let url = part["image_url"]["url"].as_str().unwrap();
                        let (mime, encoded) = url
                            .strip_prefix("data:")
                            .unwrap()
                            .split_once(";base64,")
                            .unwrap();
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .unwrap();
                        images += 1;
                        json!(format!(
                            "[{mime}, {} bytes] sha256:{}",
                            bytes.len(),
                            Sha256::digest(&bytes)
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>()
                        ))
                    }
                    kind => panic!("unexpected wire part {kind}"),
                };
            }
        }
    }
    images
}

struct ImageAssets(dekopon_model::asset::BlobReference);
impl Default for ImageAssets {
    fn default() -> Self {
        Self(DiskBlob::from_bytes(b"synthetic pixels").unwrap().into())
    }
}
impl AssetSource for ImageAssets {
    fn offers_fetch(&self) -> bool {
        true
    }
    fn fetch(&self, id: u64) -> Result<FetchedAsset, String> {
        assert_eq!(id, 1);
        Ok(FetchedAsset {
            name: "photo.png".into(),
            mime: "image/png".into(),
            data: self.0.clone(),
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_message_trace_reconstructs_requests_with_images_and_unbounded_definitions() {
    use dekopon_model::openai::OpenAiClient;
    use dekopon_test_support::CaptureLayer;
    use std::collections::BTreeMap;
    use tracing_subscriber::prelude::*;

    use dekopon_model::asset::{BlobError, BlobReference, BlobSource};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Copy)]
    enum ImageEnd {
        Released,
        Unauthorized,
    }
    struct RetainedImage {
        blob: DiskBlob,
        refusal: Mutex<Option<BlobError>>,
        reads: AtomicUsize,
    }
    impl BlobSource for RetainedImage {
        fn pin(&self) -> Result<DiskBlob, BlobError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            match *self.refusal.lock() {
                Some(error) => Err(error),
                None => Ok(self.blob.clone()),
            }
        }
    }
    struct DescribedRuntime {
        runtime: SyntheticRuntime,
        image: Arc<RetainedImage>,
        ending: ImageEnd,
    }
    impl ScriptRuntime for DescribedRuntime {
        fn capability_calls_used(&self) -> u32 {
            self.runtime.capability_calls_used()
        }
        fn run_script(&self, script: &str) -> ScriptOutcome {
            assert_eq!(script, "release image");
            match self.ending {
                ImageEnd::Released => self.image.blob.reclaim().unwrap(),
                ImageEnd::Unauthorized => {
                    *self.image.refusal.lock() = Some(BlobError::Unauthorized)
                }
            }
            self.runtime.run_script(script)
        }
        fn command_words(&self) -> Vec<String> {
            vec!["synthetic".into()]
        }
        fn command_word_help(&self) -> BTreeMap<String, String> {
            BTreeMap::from([("synthetic".into(), "detailed help ".repeat(500))])
        }
    }
    for ending in [ImageEnd::Released, ImageEnd::Unauthorized] {
        let call = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"fetch-1","type":"function","function":{"name":"fetch_chat_asset","arguments":"{\"id\":1}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;
        let answer = "data: {\"choices\":[{\"delta\":{\"content\":\"seen\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let release = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"release-1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"release image\"}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;
        let server = LoopbackServer::sequence([
            response(call),
            response(release),
            response(answer),
            response(answer),
        ]);
        let image = Arc::new(RetainedImage {
            blob: DiskBlob::from_bytes(b"synthetic pixels").unwrap(),
            refusal: Mutex::new(None),
            reads: AtomicUsize::new(0),
        });
        let assets = ImageAssets(BlobReference::new(
            Arc::clone(&image) as Arc<dyn BlobSource>,
            16,
            1,
        ));
        let retained = Arc::clone(&image);
        let timeout = Duration::from_secs(5);
        let client = OpenAiClient::new(server.url(), "scripted-model", None, timeout)
            .unwrap()
            .with_streaming(true);
        let (_sender, receiver) = tokio::sync::watch::channel(false);
        let bridge = BlockingModel::new(
            Arc::new(ModelClient::OpenAiCompatible(client)),
            tokio::runtime::Handle::current(),
            receiver,
            timeout,
        );
        let capture = CaptureLayer::workspace();
        let captured = capture.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = tracing_subscriber::registry().with(captured).set_default();
            let runtime = DescribedRuntime {
                runtime: SyntheticRuntime::default(),
                image,
                ending,
            };
            let mut history = History::default();
            for prompt in ["Look at chat-asset:1", "What did you see?"] {
                run_prompt_session(
                    &bridge,
                    &runtime,
                    SessionInputs::new(
                        prompt,
                        PromptLimits {
                            max_steps: 3,
                            max_capability_calls: 1,
                        },
                    )
                    .with_assets(&assets),
                    &mut history,
                )
                .unwrap();
            }
            assert_eq!(*runtime.runtime.0.lock(), ["release image"]);
        })
        .await
        .unwrap();
        let sent: Vec<Value> = (0..4).map(|_| body(&server.request_text())).collect();
        server.join();
        assert_eq!(retained.reads.load(Ordering::Relaxed), 2);
        let prompts: Vec<_> = capture
            .records()
            .into_iter()
            .filter(|record| record.field("audit.event") == Some("agent.model.prompt"))
            .collect();
        assert_eq!(
            prompts
                .iter()
                .map(|record| record.field("transcript.scope").unwrap())
                .collect::<Vec<_>>(),
            ["full", "delta", "full", "full"]
        );
        assert_eq!(assert_trace_reconstructs_requests(capture, sent), 1);
    }
}

fn assert_trace_reconstructs_requests(
    capture: dekopon_test_support::CaptureLayer,
    sent: Vec<Value>,
) -> usize {
    use dekopon_test_support::Record;
    let mut messages = Vec::new();
    let mut definitions = Value::Null;
    let mut exchange = serde_json::Map::new();
    let mut rebuilt = Vec::new();
    for record in capture.records() {
        match record {
            Record::Span {
                name: "model.complete",
                ..
            } => {
                for key in ["model.dialect", "tool_choice", "parallel_tool_calls"] {
                    if let Some(value) = record.field(key) {
                        exchange.insert(
                            key.into(),
                            if key == "parallel_tool_calls" {
                                serde_json::from_str(value).unwrap()
                            } else {
                                json!(value)
                            },
                        );
                    }
                }
            }
            Record::Event { .. } if record.field("audit.event") == Some("agent.model.prompt") => {
                if record.field("transcript.scope") == Some("full") {
                    messages.clear();
                    let encoded = record.field("gen_ai.tool.definitions").unwrap();
                    assert!(encoded.len() > 4096);
                    definitions = serde_json::from_str(encoded).unwrap();
                } else {
                    assert!(record.field("gen_ai.tool.definitions").is_none());
                }
                messages.extend(
                    serde_json::from_str::<Vec<Value>>(record.field("messages").unwrap()).unwrap(),
                );
                assert_eq!(
                    messages.len(),
                    record
                        .field("message.count")
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                );
                rebuilt.push(json!({"messages":messages,"tools":definitions,"exchange":exchange}));
                exchange.clear();
            }
            Record::Span { .. } | Record::Event { .. } => {}
        }
    }
    assert_eq!(rebuilt.len(), sent.len());
    let mut images = 0;
    for (reconstructed, mut sent) in rebuilt.into_iter().zip(sent) {
        images += summarize_wire_images(&mut sent["messages"]);
        let tools: Vec<Value> = sent["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"].clone())
            .collect();
        let mut exchange = json!({"model.dialect":"chat-completions"});
        for key in ["tool_choice", "parallel_tool_calls"] {
            if let Some(value) = sent.get(key) {
                exchange[key] = value.clone();
            }
        }
        assert_eq!(
            reconstructed,
            json!({"messages":sent["messages"],"tools":tools,"exchange":exchange})
        );
    }
    images
}

#[tokio::test(flavor = "multi_thread")]
async fn a_steered_cancellation_repeats_the_full_prompt_with_resolved_image_digests() {
    use dekopon_agent::prompt::{Handoff, Seal, Steer, SteerSource};
    use dekopon_model::{
        error::InferenceError,
        model::{AssistantTurn, ChatModel, CompletionOptions, ModelMessage, ModelTool},
        openai::OpenAiClient,
        stream::TurnEvent,
    };
    use dekopon_test_support::{CaptureLayer, Record};
    use std::{
        ops::ControlFlow,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tracing_subscriber::prelude::*;

    #[derive(Default)]
    struct Mailbox(Mutex<Vec<Steer>>);
    impl SteerSource for Mailbox {
        fn take(&self) -> Vec<Steer> {
            std::mem::take(&mut *self.0.lock())
        }
        fn seal_or_take(&self) -> Handoff {
            Handoff::Sealed(self.seal())
        }
        fn seal(&self) -> Seal {
            Seal::Sealed {
                leftover: self.0.lock().len(),
            }
        }
    }
    struct CancelBeforeImage {
        bridge: BlockingModel,
        cancel: tokio::sync::watch::Sender<bool>,
        calls: AtomicUsize,
        mailbox: Mailbox,
    }
    impl ChatModel for CancelBeforeImage {
        fn complete(
            &self,
            messages: &[ModelMessage],
            tools: &[ModelTool],
            options: &CompletionOptions,
            on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        ) -> Result<AssistantTurn, InferenceError> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => {}
                1 => {
                    self.mailbox
                        .0
                        .lock()
                        .push(Steer::Notice("Describe the image.".into()));
                    self.cancel.send(true).unwrap();
                }
                2 => self.cancel.send(false).unwrap(),
                call => panic!("unexpected call {call}"),
            }
            self.bridge.complete(messages, tools, options, on_event)
        }
    }
    let call = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"fetch-1","type":"function","function":{"name":"fetch_chat_asset","arguments":"{\"id\":1}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;
    let answer = "data: {\"choices\":[{\"delta\":{\"content\":\"seen\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let server = LoopbackServer::sequence([response(call), response(answer)]);
    let timeout = Duration::from_secs(5);
    let client = OpenAiClient::new(server.url(), "scripted-model", None, timeout)
        .unwrap()
        .with_streaming(true);
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    let model = CancelBeforeImage {
        bridge: BlockingModel::new(
            Arc::new(ModelClient::OpenAiCompatible(client)),
            tokio::runtime::Handle::current(),
            receiver,
            timeout,
        ),
        cancel,
        calls: AtomicUsize::new(0),
        mailbox: Mailbox::default(),
    };
    let capture = CaptureLayer::workspace();
    let captured = capture.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = tracing_subscriber::registry().with(captured).set_default();
        let outcome = run_prompt_session(
            &model,
            &SyntheticRuntime::default(),
            SessionInputs::new(
                "Look at chat-asset:1",
                PromptLimits {
                    max_steps: 2,
                    max_capability_calls: 1,
                },
            )
            .with_assets(&ImageAssets::default())
            .with_steering(&model.mailbox),
            &mut History::default(),
        )
        .unwrap();
        assert_eq!(outcome.answer, "seen");
        assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    })
    .await
    .unwrap();
    let first = body(&server.request_text());
    let mut resumed = body(&server.request_text());
    server.join();
    assert_eq!(summarize_wire_images(&mut resumed["messages"]), 1);
    let prompts: Vec<Record> = capture
        .records()
        .into_iter()
        .filter(|record| record.field("audit.event") == Some("agent.model.prompt"))
        .collect();
    assert_eq!(prompts.len(), 3);
    assert_eq!(prompts[1].field("transcript.scope"), Some("delta"));
    assert!(
        prompts[1]
            .field("messages")
            .unwrap()
            .contains("[image/png, 16 bytes]")
    );
    assert!(!prompts[1].field("messages").unwrap().contains("sha256:"));
    assert_eq!(prompts[2].field("transcript.scope"), Some("full"));
    assert_eq!(
        serde_json::from_str::<Value>(prompts[2].field("messages").unwrap()).unwrap(),
        resumed["messages"],
    );
    let definitions: Value =
        serde_json::from_str(prompts[2].field("gen_ai.tool.definitions").unwrap()).unwrap();
    let sent_definitions: Vec<Value> = resumed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"].clone())
        .collect();
    assert_eq!(definitions, json!(sent_definitions));
    assert_eq!(first["tools"], resumed["tools"]);
}
