use super::*;
use crate::{
    bootstrap::SessionBootstrap,
    history::History,
    runtime::{ScriptRuntime, ShellRuntime},
    session::{PromptLimits, SessionEngine},
};
use dekopon_model::{model::*, usage::AttemptRecorder};
use dekopon_shell::{CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandRun};
use serde_json::{Value, json};

struct Invoker;
impl CapabilityInvoker for Invoker {
    fn granted(&self) -> Vec<String> {
        vec!["test.read".into(), "test.other".into()]
    }
    fn describe(&self, c: &str) -> Option<CapabilityDescription> {
        Some(CapabilityDescription {
            capability: c.into(),
            description: "PRIVATE TITLE secret <@U1>".into(),
            input_schema: json!({"type":"object"}),
        })
    }
    fn command_words(&self) -> Vec<String> {
        vec!["probe".into()]
    }
    fn run_command(&self, _: &str, args: &[String], _: Option<&str>) -> Option<CommandRun> {
        Some(if args == ["--help"] {
            CommandRun::Rendered {
                stdout: "private help".into(),
                stderr: String::new(),
                status: 0,
            }
        } else {
            CommandRun::Proposed {
                capability: "test.read".into(),
                input: json!({"credential":"NEVER STATUS"}),
            }
        })
    }
    fn invoke(
        &self,
        _: &str,
        _: Value,
        _: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        CapabilityCallResult::Succeeded(json!({"private":"NEVER STATUS"}))
    }
}
struct Model(Mutex<bool>);
impl ChatModel for Model {
    fn complete(
        &self,
        _: &[ModelMessage],
        _: &[ModelTool],
        recorder: &dyn AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
        #[allow(
            clippy::redundant_closure_call,
            reason = "fixture early returns must still record usage before propagation"
        )]
        let result: Result<AssistantTurn, ModelError> = (|| {
            if std::mem::replace(&mut *self.0.lock().unwrap(), true) {
                return Err(ModelError::NoChoices);
            }
            Ok(AssistantTurn { content:None, tool_calls:vec![ModelToolCall { id:"script".into(), kind:"function".into(), function:ModelFunctionCall { name:"bash".into(), arguments:json!({"script":"probe --help; f() { probe --private=title; }; for n in 1 2; do f; done; test.other --secret=NEVER"}).to_string() } }], usage:None, replay_items:Vec::new() })
        })();
        if let Ok(turn) = &result
            && let Some(usage) = turn.usage
        {
            recorder.observe(
                attempt,
                dekopon_model::usage::UsageObservation {
                    usage,
                    invalid: [false; 5],
                },
            )?;
        }
        result
    }
}
#[test]
fn actual_nested_submissions_have_distinct_operations_and_no_payload_labels() {
    let publisher = ActivityPublisher::default();
    let runtime = ShellRuntime {
        invoker: Invoker,
        limits: Default::default(),
        curl_capability: None,
    };
    let labels = BTreeMap::from([
        (
            "test.read".into(),
            ActivityLabel::sanitized("Fetching Wikipedia page"),
        ),
        (
            "not.granted".into(),
            ActivityLabel::sanitized("ungranted label"),
        ),
    ]);
    let model = Model(Mutex::new(false));
    let mut history = History::default();
    assert!(
        SessionEngine::new(&model, &runtime)
            .run(
                SessionBootstrap::new(
                    "request",
                    PromptLimits {
                        max_steps: 3,
                        max_capability_calls: 8
                    },
                    "fixture"
                )
                .with_activity(&publisher, &labels),
                &mut history
            )
            .is_err()
    );
    let events = publisher.0.queue.lock().unwrap();
    assert_eq!(
        events.len(),
        6,
        "help and builtins are not capability executions"
    );
    for (i, pair) in events.as_slices().0.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(pair[0].operation, i as u32 + 1);
        assert_eq!(pair[1].operation, pair[0].operation);
        assert_eq!(pair[0].phase, ActivityPhase::Submitted);
        assert_eq!(pair[1].phase, ActivityPhase::Finished);
        assert_eq!(pair[1].outcome, Some(ExecutionOutcome::Succeeded));
        assert_eq!(pair[0].job, history.turns()[0].job);
        assert_eq!(
            pair[0].label.as_str(),
            if i < 2 {
                "Fetching Wikipedia page"
            } else {
                "Running capability"
            }
        );
    }
    let text = format!("{events:?}");
    for forbidden in ["NEVER", "PRIVATE", "credential", "test.read", "not.granted"] {
        assert!(!text.contains(forbidden));
    }
}
#[test]
fn flood_is_bounded_and_terminal_seal_survives_full_or_contended_queue() {
    let p = ActivityPublisher::default();
    let runtime = ShellRuntime {
        invoker: Invoker,
        limits: Default::default(),
        curl_capability: None,
    };
    let emitter = p.bind(
        "opaque-job".into(),
        &BTreeMap::new(),
        &runtime.capability_snapshot().unwrap(),
    );
    for i in 0..10000 {
        emitter.emit(i, "test.read", ActivityPhase::Submitted, None);
    }
    assert!(p.0.queue.lock().unwrap().len() <= 32);
    assert_eq!(p.latest().unwrap().operation, 9999);
    // Try-lock publication never blocks a script on a slow consumer.
    let queue = p.0.queue.lock().unwrap();
    emitter.emit(10001, "test.read", ActivityPhase::Submitted, None);
    p.seal();
    drop(queue);
    emitter.emit(10002, "test.read", ActivityPhase::Submitted, None);
    assert!(p.latest().is_none());
}
#[test]
fn labels_remove_controls_directional_marks_and_bound_utf8() {
    let label = ActivityLabel::sanitized(&format!("\u{061c}\u{202e}\n{}\u{2066}", "é".repeat(100)));
    assert_eq!(label.as_str().len(), 80);
    assert!(label.as_str().chars().all(|c| c == 'é'));
    assert_eq!(
        ActivityLabel::sanitized("\n\u{200d}\u{feff}").as_str(),
        "Running capability"
    );
}

/// The configuration gate accepts exactly the labels the renderer keeps whole.
///
/// A mirror that accepts what the authority truncates is worse than no mirror: the gate measured
/// the trimmed text while the renderer bounded the untrimmed one, so two leading spaces bought a
/// label that passed startup validation and then lost its last two characters in the channel.
#[test]
fn the_label_gate_accepts_exactly_what_the_renderer_keeps_whole() {
    let longest = "a".repeat(MAX_ACTIVITY_LABEL_BYTES);
    for (raw, rendered) in [
        (longest.clone(), longest.clone()),
        (format!("  {longest}  "), longest.clone()),
        (format!("\u{202e}{longest}"), longest.clone()),
        (
            " Reading the shared record ".to_owned(),
            "Reading the shared record".to_owned(),
        ),
    ] {
        assert!(crate::activity::label_is_renderable(&raw), "{raw:?}");
        assert_eq!(
            ActivityLabel::sanitized(&raw).as_str(),
            rendered,
            "the gate accepted a label the renderer then truncated: {raw:?}"
        );
    }
    for raw in [format!("{longest}b"), "   ".to_owned(), String::new()] {
        assert!(
            !crate::activity::label_is_renderable(&raw),
            "the gate accepted a label the renderer cannot keep whole: {raw:?}"
        );
    }
}
