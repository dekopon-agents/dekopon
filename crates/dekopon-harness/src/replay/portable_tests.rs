use super::*;
use crate::{
    context::ContextPolicy,
    history::{
        DeliveryDisposition, Excerpt, ExecutionOutcome, ExecutionProvenance, ExecutionRecord,
        JobRecord, ToolGroup, ToolResult,
    },
};
use dekopon_model::model::{AssistantTurn, ModelMessage, ModelToolCall, assistant_message};
use serde_json::json;

fn tool(id: &str, name: &str, arguments: Value) -> ModelToolCall {
    serde_json::from_value(json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}})).unwrap()
}
fn group(call: u32, tool: ModelToolCall, result: &str) -> ToolGroup {
    ToolGroup {
        call,
        results: vec![ToolResult {
            id: tool.id.clone(),
            result: Excerpt::new(result, crate::history::MAX_EXCERPT_BYTES),
        }],
        calls: vec![tool],
        omitted: false,
        provenance: None,
    }
}
fn execution(job: &str) -> ExecutionRecord {
    ExecutionRecord {
        job: job.into(),
        call: 1,
        tool: "same-id".into(),
        sequence: 1,
        capability: "posts.count".into(),
        provenance: ExecutionProvenance::BrokerObserved,
        invocation: Some("invocation-fixture".into()),
        evidence: vec!["sha256:fixture".into()],
        outcome: ExecutionOutcome::Succeeded,
        result: Some(Excerpt::new("42", 4096)),
    }
}
fn prompt(turn: u32, revision: u64, scope: &str, messages: &[ModelMessage]) -> Value {
    json!({"trace_id":"portable", "audit_event":"agent.model.prompt", "job_id":"current-job", "model_turn":turn,"transcript_version":2,"context_revision":revision,"transcript_scope":scope,"messages":serde_json::to_string(messages).unwrap(),"message_count":messages.len()})
}
fn answer_record(turn: u32, text: &str, calls: &[ModelToolCall]) -> Value {
    json!({"trace_id":"portable", "audit_event":"agent.model.answer", "model_turn":turn, "answer":text,"tool_calls":serde_json::to_string(calls).unwrap()})
}
fn call_record(turn: u32) -> Value {
    json!({"trace_id":"portable", "audit_event":"accounting.model.call","job_id":"current-job","call_sequence":turn,"model_turn":turn,"model_kind":"chat","usage_input_tokens":10,"usage_output_tokens":2,"usage_total_tokens":12})
}
fn fixture() -> (Vec<Value>, Vec<ModelMessage>) {
    let mut old = JobRecord::new("old-job".into(), "earlier question");
    old.generated = Some("same answer".into());
    old.delivery = DeliveryDisposition::Accepted {
        text: "same answer".into(),
    };
    old.groups = vec![group(
        1,
        tool("same-id", "bash", json!({"script":"old.read"})),
        "old result\n[exit code: 0]",
    )];
    old.executions = vec![execution("old-job")];
    let history = History::from_turns(HistoryLimits::default(), [old]);
    let mut first = vec![ModelMessage::system("Be brief.")];
    first.extend(crate::context::WindowContext.select(&history));
    first.push(ModelMessage::user("follow-up"));
    let script = tool("same-id", "bash", json!({"script":"posts.count"}));
    let switch = tool("same-id", "select_model", json!({"model":"second"}));
    let delta = vec![
        assistant_message(&AssistantTurn {
            content: None,
            tool_calls: vec![script.clone()],
            usage: None,
            replay_items: vec![json!({"opaque":"must not be serialized"})],
        }),
        ModelMessage::tool("same-id", "42\n[exit code: 0]"),
    ];
    let mut current = JobRecord::new("current-job".into(), "follow-up");
    current.groups = vec![
        group(1, script.clone(), "42\n[exit code: 0]"),
        group(2, switch.clone(), "Model selection applied."),
    ];
    current.executions = vec![execution("current-job")];
    let mut rebuilt = vec![ModelMessage::system("second model bootstrap")];
    rebuilt.extend(crate::context::WindowContext.select(&history));
    crate::context::replay_job(&current, &mut rebuilt);
    let records = vec![
        prompt(1, 0, "full", &first),
        answer_record(1, "", &[script]),
        call_record(1),
        prompt(2, 0, "delta", &delta),
        answer_record(2, "", &[switch]),
        call_record(2),
        prompt(3, 1, "full", &rebuilt),
        answer_record(3, "same answer", &[]),
        call_record(3),
    ];
    (records, first)
}

#[test]
fn persistent_tool_history_and_model_switch_revisions_reconstruct_without_recounting() {
    let (mut records, first) = fixture();
    records.extend(records.clone()); // repeated exports are idempotent, independently of row order
    records.reverse();
    let recorded = RecordedSession::from_records("portable", &records).unwrap();
    assert_eq!(recorded.prompt, "follow-up");
    assert!(
        recorded.history.is_empty(),
        "a lossy text-pair projection is not history"
    );
    assert_eq!(recorded.contexts.len(), 3);
    assert_eq!(
        recorded
            .contexts
            .iter()
            .map(|c| c.revision)
            .collect::<Vec<_>>(),
        vec![Some(0), Some(0), Some(1)]
    );
    assert_eq!(
        serde_json::to_value(&recorded.contexts[0].messages).unwrap(),
        serde_json::to_value(&first).unwrap()
    );
    assert_eq!(recorded.turns.len(), 3);
    assert_eq!(recorded.scripts(), ["posts.count"]);
    assert_eq!(
        recorded.turns[0].tool_calls[0].result.as_deref(),
        Some("42\n[exit code: 0]")
    );
    assert_eq!(
        recorded.turns[1].tool_calls[0].result.as_deref(),
        Some("Model selection applied.")
    );
    assert_eq!(recorded.usage().total_tokens, Some(36));
    assert_eq!(recorded.answer.as_deref(), Some("same answer"));
    let encoded = serde_json::to_string(&recorded).unwrap();
    assert!(!encoded.contains("must not be serialized"));
    let decoded: RecordedSession = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, recorded);

    struct Model {
        expected: Vec<ModelMessage>,
        calls: Mutex<u32>,
    }
    impl ChatModel for Model {
        fn complete(
            &self,
            messages: &[ModelMessage],
            _: &[dekopon_model::model::ModelTool],
            recorder: &dyn dekopon_model::usage::AttemptRecorder,
        ) -> Result<AssistantTurn, dekopon_model::model::ModelError> {
            recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                let portable: Vec<_> = messages
                    .iter()
                    .filter(|m| m.role() != "system")
                    .cloned()
                    .collect();
                assert_eq!(portable, self.expected);
                Ok(AssistantTurn {
                    content: None,
                    tool_calls: vec![tool("replay-id", "bash", json!({"script":"posts.count"}))],
                    usage: None,
                    replay_items: vec![],
                })
            } else {
                assert_eq!(
                    messages.last().unwrap().content(),
                    Some("42\n[exit code: 0]")
                );
                Ok(AssistantTurn {
                    content: Some("done".into()),
                    tool_calls: vec![],
                    usage: None,
                    replay_items: vec![],
                })
            }
        }
    }
    let model = Model {
        expected: first.into_iter().filter(|m| m.role() != "system").collect(),
        calls: Mutex::new(0),
    };
    let ledger = crate::accounting::JobAccounting::default();
    let report = replay(
        &model,
        &decoded,
        ReplayInputs {
            accounting: Some(&ledger),
            selected_model: "fixture",
            system: None,
            skills: &[],
            improvement_suggestions: false,
            live: None,
            limits: PromptLimits {
                max_steps: 3,
                max_capability_calls: 2,
            },
        },
    );
    assert_eq!(report.error, None);
    assert_eq!(report.divergence, None);
    assert_eq!(report.replayed.scripts, ["posts.count"]);
    assert_eq!(report.replayed.model_turns, 2);
    assert_eq!(
        ledger.snapshot().calls.len(),
        2,
        "recorded spend is not replay spend"
    );
}

#[test]
fn malformed_and_conflicting_revisions_name_the_failed_check() {
    let (records, _) = fixture();
    let cases = [
        (
            "backward revision",
            6,
            "context_revision",
            json!(0),
            "revision ordering",
        ),
        (
            "changed delta revision",
            3,
            "context_revision",
            json!(1),
            "revision ordering",
        ),
        (
            "unknown scope",
            3,
            "transcript_scope",
            json!("other"),
            "revision ordering",
        ),
        (
            "missing revision",
            6,
            "context_revision",
            Value::Null,
            "revision/job",
        ),
        (
            "unsupported version",
            6,
            "transcript_version",
            json!(3),
            "transcript.version",
        ),
        (
            "conflicting job",
            6,
            "job_id",
            json!("other-job"),
            "job IDs",
        ),
    ];
    for (name, index, key, value, cause) in cases {
        let mut bad = records.clone();
        bad[index][key] = value;
        let error = RecordedSession::from_records("portable", &bad)
            .unwrap_err()
            .to_string();
        assert!(error.contains(cause), "{name}: {error}");
    }
    for (index, key, value, cause) in [
        (0, "messages", json!("[]"), "conflicting prompt"),
        (1, "answer", json!("different"), "conflicting answer"),
        (2, "usage_input_tokens", json!(99), "conflicting accounting"),
    ] {
        let mut bad = records.clone();
        let mut duplicate = bad[index].clone();
        duplicate[key] = value;
        bad.push(duplicate);
        let error = RecordedSession::from_records("portable", &bad)
            .unwrap_err()
            .to_string();
        assert!(error.contains(cause), "{error}");
    }
    let mut bad = records.clone();
    bad.remove(3);
    assert!(
        RecordedSession::from_records("portable", &bad)
            .unwrap_err()
            .to_string()
            .contains("missing or out-of-order")
    );
    for (id, cause) in [
        ("orphan", "orphan or duplicate"),
        ("same-id", "delta conflicts"),
    ] {
        let mut bad = records.clone();
        let mut delta: Value = serde_json::from_str(bad[3]["messages"].as_str().unwrap()).unwrap();
        delta[1]["tool_call_id"] = json!(id);
        if id == "same-id" {
            delta[0]["tool_calls"][0]["function"]["arguments"] = json!("{}");
        }
        bad[3]["messages"] = json!(delta.to_string());
        let error = RecordedSession::from_records("portable", &bad)
            .unwrap_err()
            .to_string();
        assert!(error.contains(cause), "{error}");
    }
}

#[test]
fn full_revisions_can_trim_groups_but_cannot_change_observed_results() {
    let (mut records, _) = fixture();
    let mut full: Vec<Value> =
        serde_json::from_str(records[6]["messages"].as_str().unwrap()).unwrap();
    full.retain(|m| {
        m["tool_call_id"] != "current-job-1-0" && m["tool_calls"][0]["id"] != "current-job-1-0"
    });
    records[6]["messages"] = json!(serde_json::to_string(&full).unwrap());
    let recorded = RecordedSession::from_records("portable", &records).unwrap();
    assert_eq!(
        recorded.turns[0].tool_calls[0].result.as_deref(),
        Some("42\n[exit code: 0]")
    );
    let (mut bad, _) = fixture();
    let mut full: Vec<Value> = serde_json::from_str(bad[6]["messages"].as_str().unwrap()).unwrap();
    full.iter_mut()
        .find(|m| m["tool_call_id"] == "current-job-1-0")
        .unwrap()["content"] = json!("forged success");
    bad[6]["messages"] = json!(serde_json::to_string(&full).unwrap());
    assert!(
        RecordedSession::from_records("portable", &bad)
            .unwrap_err()
            .to_string()
            .contains("conflicting tool results")
    );
}
