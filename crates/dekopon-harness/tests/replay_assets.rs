//! Capture the real asset emitter in isolation because payload telemetry is process-global.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use dekopon_harness::{
    bootstrap::{BootstrapError, CapabilitySnapshot, SessionBootstrap},
    history::History,
    replay::{RecordedSession, ReplayInputs, replay},
    runtime::ScriptRuntime,
    session::{PromptLimits, SessionEngine},
    tools::{ASSET_TOOL_NAME, AssetSource, FetchedAsset},
};
use dekopon_model::{
    model::{AssistantTurn, ChatModel, ModelError, ModelMessage, ModelTool, ModelToolCall},
    usage::{AttemptKind, AttemptRecorder},
};
use dekopon_shell::{ExitCode, ScriptOutcome};
use serde_json::{Value, json};

#[derive(Clone)]
struct Writer(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct Model(Mutex<VecDeque<AssistantTurn>>);
impl ChatModel for Model {
    fn complete(
        &self,
        _: &[ModelMessage],
        _: &[ModelTool],
        recorder: &dyn AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        recorder.begin(AttemptKind::Adapter)?;
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ModelError::NoChoices)
    }
}
struct Runtime;
impl ScriptRuntime for Runtime {
    fn capability_snapshot(&self) -> Result<CapabilitySnapshot, BootstrapError> {
        Ok(CapabilitySnapshot::empty())
    }
    fn run_script(&self, script: &str, _: u32) -> ScriptOutcome {
        assert_eq!(script, "echo result");
        ScriptOutcome {
            output: "result".into(),
            exit_code: ExitCode::SUCCESS,
            truncated: false,
            capability_calls: 0,
            steps: 1,
        }
    }
}
struct Assets(&'static str);
impl AssetSource for Assets {
    fn fetch(&self, id: u64) -> Result<FetchedAsset, String> {
        assert_eq!(id, 1);
        Ok(FetchedAsset {
            name: "fixture".into(),
            mime: self.0.into(),
            data: b"private-attachment-bytes".to_vec(),
        })
    }
    fn is_empty(&self) -> bool {
        false
    }
}
fn answer() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".into()),
        tool_calls: vec![],
        usage: None,
        replay_items: vec![],
    }
}
fn inputs() -> ReplayInputs<'static> {
    ReplayInputs {
        accounting: None,
        selected_model: "fixture",
        system: None,
        skills: &[],
        improvement_suggestions: false,
        live: None,
        limits: PromptLimits {
            max_steps: 3,
            max_capability_calls: 2,
        },
    }
}

#[test]
fn emitted_asset_interleaving_reconstructs_with_both_results_and_without_bytes() {
    dekopon_core::set_telemetry_payloads(true);
    for mime in ["image/png", "application/pdf"] {
        for failed in [false, true] {
            let call = |id: &str, name: &str, arguments: Value| -> ModelToolCall {
                serde_json::from_value(json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}})).unwrap()
            };
            let mut turns = VecDeque::from([AssistantTurn {
                content: None,
                tool_calls: vec![
                    call("asset", ASSET_TOOL_NAME, json!({"id":1})),
                    call("script", "bash", json!({"script":"echo result"})),
                ],
                usage: None,
                replay_items: vec![],
            }]);
            if !failed {
                turns.push_back(answer());
            }
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let writer = Writer(bytes.clone());
            let subscriber = tracing_subscriber::fmt()
                .json()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish();
            let outcome = tracing::subscriber::with_default(subscriber, || {
                SessionEngine::new(&Model(Mutex::new(turns)), &Runtime).run(
                    SessionBootstrap::new("inspect", inputs().limits, "fixture")
                        .with_assets(&Assets(mime)),
                    &mut History::default(),
                )
            });
            assert_eq!(outcome.is_err(), failed);
            if let Err(error) = outcome {
                assert!(error.to_string().contains("no choices"), "{error}");
            }
            let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
            assert!(!output.contains("private-attachment-bytes"));
            let records: Vec<Value> = output
                .lines()
                .map(|line| {
                    let event: Value = serde_json::from_str(line).unwrap();
                    let mut fields = event["fields"].clone();
                    fields["trace_id"] = json!("asset");
                    fields
                })
                .collect();
            let recorded = RecordedSession::from_records("asset", &records).unwrap();
            assert_eq!(recorded.contexts.len(), 2);
            assert_eq!(
                recorded.contexts[1]
                    .messages
                    .iter()
                    .map(|m| m.role.as_str())
                    .collect::<Vec<_>>(),
                ["assistant", "tool", "user", "tool"]
            );
            assert!(
                recorded.contexts[1].messages[2]
                    .content
                    .as_ref()
                    .unwrap()
                    .contains(mime)
            );
            assert_eq!(
                recorded.turns[0].tool_calls[0].result.as_deref(),
                Some("Chat Asset #1 follows in the next message.")
            );
            assert_eq!(
                recorded.turns[0].tool_calls[1].result.as_deref(),
                Some("result\n[exit code: 0]")
            );
            let file = serde_json::to_vec(&recorded).unwrap();
            let decoded: RecordedSession = serde_json::from_slice(&file).unwrap();
            decoded.validate().unwrap();
            let report = replay(
                &Model(Mutex::new(VecDeque::from([answer()]))),
                &decoded,
                inputs(),
            );
            assert_eq!(report.error, None);
            assert_eq!(report.recorded.model_turns, Some(2));
            for id in ["asset", "script"] {
                let mut broken = records.clone();
                let delta = broken
                    .iter_mut()
                    .find(|r| r["audit.event"] == "agent.model.prompt" && r["model.turn"] == 2)
                    .unwrap();
                let mut messages: Vec<Value> =
                    serde_json::from_str(delta["messages"].as_str().unwrap()).unwrap();
                messages.retain(|m| m["tool_call_id"] != id);
                delta["messages"] = json!(serde_json::to_string(&messages).unwrap());
                assert!(
                    RecordedSession::from_records("asset", &broken)
                        .unwrap_err()
                        .to_string()
                        .contains("incomplete assistant tool group")
                );
            }
            let mut oversized = decoded.clone();
            oversized.contexts[1].messages[2].content =
                Some("x".repeat(dekopon_harness::context::MAX_GROUP_BYTES));
            assert!(
                oversized
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("tool group exceeds byte limit")
            );
        }
    }
}
