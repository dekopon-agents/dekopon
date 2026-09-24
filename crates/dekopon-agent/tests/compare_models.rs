#![allow(clippy::unwrap_used)]

use dekopon_agent::prompt::{
    History, PromptLimits, ScriptRuntime, SessionInputs, run_prompt_session,
};
use dekopon_model::{
    blocking::BlockingModel,
    codex::CodexClient,
    inference::ModelClient,
    openrouter::{OpenRouterClient, settings::Settings},
};
use dekopon_shell::{ExitCode, ScriptOutcome};
use dekopon_test_support::LoopbackServer;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
struct SyntheticRuntime(Mutex<Vec<String>>);
impl ScriptRuntime for SyntheticRuntime {
    fn run_script(&self, script: &str, _budget: u32) -> ScriptOutcome {
        self.0.lock().unwrap().push(script.to_owned());
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
            (outcome, runtime.0.into_inner().unwrap())
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
