use dekopon_agent::{ShellRuntime, memory_tools::Rung, prompt::ScriptRuntime};
use dekopon_process::{ExitCode, TreeContext};
use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, CommandProposal, Streams};
use serde::Serialize;
use serde_json::Value;

use super::{
    journal::records::{Field, Head},
    record::{ChoiceLine, RecordId},
};
use crate::config::memory as config;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Evidence {
    tool: EvidenceTool,
    outcome: EvidenceOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_status: Option<u8>,
    age_ms: u64,
    bytes: u64,
    pinned: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum EvidenceTool {
    Bash,
    #[serde(rename = "none")]
    Absent,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum EvidenceOutcome {
    Success,
    Failure,
    #[serde(rename = "none")]
    Absent,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChooseInput<'a> {
    backend: &'a str,
    model: &'a str,
    evidence: &'a [Evidence],
    labels: &'a [String],
    require_probabilities: bool,
}

const MAX_EVIDENCE: usize = 256;

struct Decider<'a, I> {
    inner: &'a I,
    capability: &'a str,
    input: Value,
}

impl<I: CapabilityInvoker> CapabilityInvoker for Decider<'_, I> {
    fn granted(&self) -> Vec<String> {
        self.inner.granted()
    }
    fn cancel_signals(&self) -> Vec<dekopon_process::CancelSignal> {
        self.inner.cancel_signals()
    }
    fn script_started(&self, tree: &mut dekopon_process::TreeContext) {
        self.inner.script_started(tree);
    }
    fn script_finished(&self) {
        self.inner.script_finished();
    }
    fn has_command_word(&self, word: &str) -> bool {
        word == "memory_decide"
    }
    fn run(
        &self,
        call: dekopon_shell::RunCall,
        streams: Streams,
        tree: &TreeContext,
    ) -> dekopon_shell::RunResult {
        if call.word != "memory_decide" {
            return dekopon_shell::render_run(call, None, streams);
        }
        let capability = self.capability.to_owned();
        let input = self.input.clone();
        let result = self.inner.invoke(
            CommandProposal::new(capability.clone(), input.clone(), None),
            streams,
            tree,
        );
        dekopon_shell::RunResult::Invoked {
            capability,
            input,
            secret_use: None,
            result,
        }
    }
    fn invoke(
        &self,
        proposal: CommandProposal,
        streams: Streams,
        tree: &TreeContext,
    ) -> CapabilityCallResult {
        self.inner.invoke(proposal, streams, tree)
    }
}

pub(crate) fn decide<I: CapabilityInvoker>(
    runtime: &ShellRuntime<I>,
    decider: &config::Decider,
    available: &[String],
    evidence: &[Evidence],
) -> Result<String, &'static str> {
    let capability = decider.capability.as_str();
    let backend = decider.backend.as_str();
    let model = decider.model.as_str();
    let require_probabilities = decider.require_probabilities;
    if !runtime.invoker.is_granted(capability) {
        return Err("not-granted");
    }
    if available.len() < 2 {
        return Err("single-label");
    }
    let input = serde_json::to_value(ChooseInput {
        backend,
        model,
        evidence: &evidence[evidence.len().saturating_sub(MAX_EVIDENCE)..],
        labels: available,
        require_probabilities,
    })
    .map_err(|_unencodable| "invalid-request")?;
    let invoker = ShellRuntime {
        invoker: Decider {
            inner: &runtime.invoker,
            capability,
            input,
        },
        limits: runtime.limits,
        calls: runtime.calls.clone(),
        threads: runtime.threads.clone(),
    };
    let result = invoker.run_script("memory_decide", None);
    if result.exit_code != ExitCode::SUCCESS {
        return Err("invocation-failed");
    }
    let response: Value =
        serde_json::from_str(result.output.trim()).map_err(|_malformed| "invalid-response")?;
    if response.get("refused") == Some(&Value::Bool(true)) {
        return Err("refused");
    }
    response
        .get("choice")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or("invalid-response")
}

pub(crate) fn evidence(records: &[Head], now_ms: u64) -> Vec<Evidence> {
    records
        .iter()
        .map(|head| {
            let outcome = head.header.spans.iter().find_map(|span| match span.field {
                Field::Result { outcome, .. } => Some(outcome),
                Field::User { .. }
                | Field::Steer { .. }
                | Field::Assistant { .. }
                | Field::Plain { .. }
                | Field::Opaque { .. }
                | Field::CallId
                | Field::RejectedTool
                | Field::Arguments { .. } => None,
            });
            let bash = head.header.spans.iter().any(|span| {
                matches!(
                    span.field,
                    Field::Arguments {
                        tool: super::journal::records::NamedTool::Bash,
                        ..
                    }
                )
            });
            Evidence {
                tool: if bash {
                    EvidenceTool::Bash
                } else {
                    EvidenceTool::Absent
                },
                outcome: match outcome {
                    None => EvidenceOutcome::Absent,
                    Some(dekopon_agent::memory::Outcome::Exit(0)) => EvidenceOutcome::Success,
                    Some(_) => EvidenceOutcome::Failure,
                },
                exit_status: match outcome {
                    Some(dekopon_agent::memory::Outcome::Exit(code)) => Some(code),
                    Some(
                        dekopon_agent::memory::Outcome::Refused
                        | dekopon_agent::memory::Outcome::Failed,
                    )
                    | None => None,
                },
                age_ms: now_ms.saturating_sub(head.header.at_ms),
                bytes: head.header.body_len,
                pinned: head.header.pin.is_some(),
            }
        })
        .collect()
}

pub(crate) fn choose<I: CapabilityInvoker>(
    runtime: &ShellRuntime<I>,
    decider: &config::Decider,
    record: &Head,
    labels: &[Rung],
    evidence: &[Evidence],
    at_ms: u64,
) -> Option<ChoiceLine> {
    let names: Vec<String> = labels.iter().map(Rung::to_string).collect();
    let outcome = decide(runtime, decider, &names, evidence);
    let chosen = outcome
        .as_ref()
        .ok()
        .and_then(|choice| choice.parse::<Rung>().ok())
        .filter(|rung| labels.contains(rung));
    tracing::info!(
        event = "gateway_memory_choice",
        memory.record.id = record.header.id.0.get(),
        choice = chosen
            .map(|rung| rung.to_string())
            .as_deref()
            .unwrap_or("none"),
        reason = match &outcome {
            Ok(_) if chosen.is_some() => "decider",
            Ok(_) => "invalid-choice",
            Err(reason) => reason,
        },
        "gateway.memory.choice"
    );
    chosen.map(|choice| ChoiceLine {
        at_ms,
        record: RecordId(record.header.id.0),
        choice,
    })
}
