use super::*;
use crate::{
    bootstrap::{CapabilitySnapshot, SessionBootstrap},
    checkpoint::{
        Checkpoint, CheckpointError, CheckpointStore, MemoryCheckpointStore, SaveReceipt,
    },
    history::History,
    runtime::ScriptRuntime,
    session::{CancellationProbe, PromptError, PromptLimits, SessionEngine},
};
use dekopon_broker_protocol::{
    BrokerClient, BrokerRequest, BrokerResponse, ControlDecision, ControlProposal, FrameLimits,
    ProtocolVersion, RequestEnvelope, ResponseEnvelope, read_frame, write_frame,
};
use dekopon_model::model::{
    AssistantTurn, ModelError, ModelFunctionCall, ModelMessage, ModelUsage, assistant_message,
};
use dekopon_shell::{ExitCode, ScriptOutcome};
use std::{
    collections::VecDeque,
    os::unix::fs::{MetadataExt, PermissionsExt},
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

type CapturedRequest = (Vec<ModelMessage>, Vec<ModelTool>, CompletionOptions);

#[derive(Default)]
struct Model {
    turns: Mutex<VecDeque<Result<AssistantTurn, ModelError>>>,
    requests: Mutex<Vec<CapturedRequest>>,
    default_only: bool,
}
impl Model {
    fn new(turns: Vec<AssistantTurn>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into_iter().map(Ok).collect()),
            ..Self::default()
        })
    }
}
impl ChatModel for Model {
    fn complete(
        &self,
        m: &[ModelMessage],
        t: &[ModelTool],
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        self.complete_with(m, t, &CompletionOptions::default(), recorder)
    }
    fn supports_effort(&self, effort: Effort) -> bool {
        !self.default_only || effort == Effort::ProviderDefault
    }
    fn complete_with(
        &self,
        m: &[ModelMessage],
        t: &[ModelTool],
        o: &CompletionOptions,
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<AssistantTurn, ModelError> {
        let attempt = recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
        #[allow(
            clippy::redundant_closure_call,
            reason = "fixture early returns must still record usage before propagation"
        )]
        let result: Result<AssistantTurn, ModelError> = (|| {
            self.validate_options(o)?;
            self.requests
                .lock()
                .unwrap()
                .push((m.to_vec(), t.to_vec(), o.clone()));
            self.turns
                .lock()
                .unwrap()
                .pop_front()
                .expect("no unexpected inference")
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
fn call(id: &str, name: &str, args: serde_json::Value) -> ModelToolCall {
    ModelToolCall {
        id: id.into(),
        kind: "function".into(),
        function: ModelFunctionCall {
            name: name.into(),
            arguments: args.to_string(),
        },
    }
}
fn batch(calls: Vec<ModelToolCall>) -> AssistantTurn {
    AssistantTurn {
        content: None,
        tool_calls: calls,
        usage: Some(ModelUsage {
            input_tokens: Some(10),
            ..Default::default()
        }),
        replay_items: vec![json!({"type":"reasoning","encrypted_content":"opaque-sentinel"})],
    }
}
fn select(id: &str, model: &str) -> AssistantTurn {
    batch(vec![call(id, SELECT_MODEL_TOOL, json!({"model":model}))])
}
fn answer() -> AssistantTurn {
    AssistantTurn {
        content: Some("done".into()),
        tool_calls: vec![],
        usage: None,
        replay_items: vec![],
    }
}
#[derive(Default)]
struct Runtime(AtomicU32);
impl ScriptRuntime for Runtime {
    fn run_script(&self, _: &str, _: u32) -> ScriptOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        ScriptOutcome {
            output: "observed output".into(),
            exit_code: ExitCode::SUCCESS,
            truncated: false,
            capability_calls: 1,
            steps: 1,
        }
    }
    fn capability_snapshot(&self) -> Result<CapabilitySnapshot, crate::bootstrap::BootstrapError> {
        Ok(CapabilitySnapshot::empty())
    }
}
struct Registry {
    a: Arc<Model>,
    b: Arc<Model>,
    fail_b: bool,
}
impl ModelRegistry for Registry {
    fn candidates(&self) -> Vec<ControlTarget> {
        ["a", "b"]
            .into_iter()
            .map(|m| ControlTarget {
                model: m.parse().unwrap(),
                efforts: vec![
                    Effort::ProviderDefault,
                    Effort::Low,
                    Effort::Medium,
                    Effort::High,
                ],
            })
            .collect()
    }
    fn prepare(&self, s: &ModelSelection) -> Result<PreparedModel, PreparationError> {
        if self.fail_b && s.model.as_str() == "b" {
            return Err(PreparationError::Unavailable);
        }
        let client = match s.model.as_str() {
            "a" => self.a.clone(),
            "b" => self.b.clone(),
            _ => return Err(PreparationError::UnknownModel),
        };
        Ok(PreparedModel {
            identity: ModelIdentity {
                configured: Some(s.model.clone()),
                backend: if s.model.as_str() == "a" {
                    "responses"
                } else {
                    "chat-completions"
                }
                .into(),
                model: format!("wire-{}", s.model),
                effort: s.effort,
            },
            client,
            accepts_images: false,
        })
    }
}
fn selection(model: &str) -> ModelSelection {
    ModelSelection {
        model: model.parse().unwrap(),
        effort: Effort::ProviderDefault,
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    client: BrokerClient,
    scope: ControlScope,
    epoch: SurfaceEpoch,
    seen: Arc<Mutex<Vec<ControlProposal>>>,
}
impl Fixture {
    fn new(
        outcomes: Vec<ControlOutcome>,
        change_epoch: bool,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broker.sock");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = {
            let _enter = rt.enter();
            tokio::net::UnixListener::bind(&path).unwrap()
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let client = BrokerClient::new(
            &path,
            std::fs::metadata(&path).unwrap().uid(),
            FrameLimits::default(),
        )
        .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_server = seen.clone();
        rt.spawn(async move {
            for outcome in outcomes {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request: RequestEnvelope = read_frame(&mut stream, FrameLimits::default())
                    .await
                    .unwrap();
                let BrokerRequest::AuthorizeControl {
                    proposal,
                    attestation,
                } = request.request
                else {
                    panic!("only authenticated control exchange expected")
                };
                seen_server.lock().unwrap().push(proposal.clone());
                if let Some(cancel) = &cancel {
                    cancel.store(true, Ordering::SeqCst);
                }
                write_frame(
                    &mut stream,
                    &ResponseEnvelope {
                        api_version: ProtocolVersion::V1Alpha3,
                        response: BrokerResponse::ControlDecision {
                            decision: Box::new(ControlDecision {
                                proposal,
                                attestation,
                                surface_epoch: if change_epoch { "changed" } else { "epoch" }
                                    .parse()
                                    .unwrap(),
                                decision_ref: format!("sha256:{}", "1".repeat(64)),
                                outcome,
                            }),
                        },
                    },
                    FrameLimits::default(),
                )
                .await
                .unwrap();
            }
        });
        Self {
            _dir: dir,
            rt,
            client,
            seen,
            epoch: "epoch".parse().unwrap(),
            scope: ControlScope {
                agent: "agent".parse().unwrap(),
                job: crate::checkpoint::opaque_id().parse().unwrap(),
                session: "session".parse().unwrap(),
                request: "request".parse().unwrap(),
                generation: "generation".parse().unwrap(),
            },
        }
    }
    fn controls<'a>(&self, registry: &'a Registry, spent: u32, max: u32) -> SessionControls<'a> {
        SessionControls::new(
            registry,
            selection("a"),
            self.client
                .control_client(self.scope.clone(), self.epoch.clone(), None, spent)
                .unwrap(),
            self.rt.handle().clone(),
            max,
        )
        .unwrap()
    }
    fn inputs<'a>(&'a self, controls: &'a SessionControls<'a>, steps: u32) -> SessionBootstrap<'a> {
        SessionBootstrap::new(
            "question",
            PromptLimits {
                max_steps: steps,
                max_capability_calls: 2,
            },
            "wire-a",
        )
        .with_scope("trusted-scope")
        .with_surface_epoch(&self.epoch)
        .with_controls(controls)
    }
}
fn saved(store: &dyn CheckpointStore, f: &Fixture) -> Checkpoint {
    store.load(f.scope.job.as_str()).unwrap()
}

#[test]
fn mixed_and_multiple_controls_refuse_every_correlated_tool_without_execution() {
    let a = Model::new(vec![
        batch(vec![
            call("script", "bash", json!({"script":"echo hi"})),
            call("switch", SELECT_MODEL_TOOL, json!({"model":"b"})),
        ]),
        batch(vec![
            call("one", SET_EFFORT_TOOL, json!({"effort":"high"})),
            call("two", SELECT_MODEL_TOOL, json!({"model":"b"})),
        ]),
        answer(),
    ]);
    let registry = Registry {
        a: a.clone(),
        b: Model::new(vec![]),
        fail_b: false,
    };
    let f = Fixture::new(vec![], false, None);
    let controls = f.controls(&registry, 0, 4);
    let store = Arc::new(MemoryCheckpointStore::default());
    let runtime = Runtime::default();
    SessionEngine::new(a.as_ref(), &runtime)
        .with_checkpoint_store(store.clone())
        .run(f.inputs(&controls, 4), &mut History::default())
        .unwrap();
    assert_eq!(runtime.0.load(Ordering::SeqCst), 0);
    assert!(f.seen.lock().unwrap().is_empty());
    let cp = saved(store.as_ref(), &f);
    assert_eq!(cp.state.spent.control_attempts, 3);
    assert!(
        cp.state
            .transitions
            .iter()
            .all(|t| t.outcome == TransitionOutcome::BatchRefused)
    );
    assert!(cp.record.groups.iter().all(|g| g.complete()));
}

#[test]
fn cross_provider_switch_rebuilds_identity_and_portable_context_without_resetting_budgets() {
    let a = Model::new(vec![
        batch(vec![call("script", "bash", json!({"script":"echo hi"}))]),
        select("switch", "b"),
    ]);
    let b = Model::new(vec![answer()]);
    let registry = Registry {
        a: a.clone(),
        b: b.clone(),
        fail_b: false,
    };
    let f = Fixture::new(vec![ControlOutcome::Admitted], false, None);
    let controls = f.controls(&registry, 0, 4);
    let store = Arc::new(MemoryCheckpointStore::default());
    let runtime = Runtime::default();
    let options = CompletionOptions::default().with_prompt_cache_key("old-lane");
    let exit = SessionEngine::new(a.as_ref(), &runtime)
        .with_checkpoint_store(store.clone())
        .run(
            f.inputs(&controls, 4).with_options(&options),
            &mut History::default(),
        )
        .unwrap();
    assert_eq!(
        (
            exit.model_turns,
            exit.script_calls,
            exit.capability_invocations
        ),
        (3, 1, 1)
    );
    let cp = saved(store.as_ref(), &f);
    assert_eq!(cp.model, "wire-b");
    assert_eq!(cp.state.accounting.calls.len(), 3);
    assert_eq!(cp.state.transitions[0].outcome, TransitionOutcome::Applied);
    assert_eq!(cp.state.transitions[0].from.backend, "responses");
    assert_eq!(
        cp.state.transitions[0].to.as_ref().unwrap().backend,
        "chat-completions"
    );
    let requests = b.requests.lock().unwrap();
    let (messages, tools, options) = &requests[0];
    assert_ne!(options.prompt_cache_key(), Some("old-lane"));
    assert!(tools.iter().any(|t| t.name == SELECT_MODEL_TOOL));
    let system = messages
        .iter()
        .filter(|m| m.role() == "system")
        .filter_map(ModelMessage::content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(system.contains("wire-b"));
    assert!(!system.contains("wire-a"));
    for group in &cp.record.groups {
        let mut calls = group.calls.clone();
        for (index, call) in calls.iter_mut().enumerate() {
            call.id = format!("{}-{}-{index}", cp.record.job, group.call);
        }
        let portable = assistant_message(&AssistantTurn {
            content: None,
            tool_calls: calls,
            usage: None,
            replay_items: vec![],
        });
        assert!(
            messages.contains(&portable),
            "opaque replay must not survive rebuilding"
        );
    }
    assert!(
        messages
            .iter()
            .any(|m| m.content().is_some_and(|s| s.contains("observed output")))
    );
    assert!(
        !serde_json::to_string(&cp)
            .unwrap()
            .contains("opaque-sentinel")
    );
}

#[test]
fn same_target_unknown_unsupported_and_denied_switches_preserve_the_old_selection_and_sequence_gaps()
 {
    let a = Model::new(vec![
        select("noop", "a"),
        select("unknown", "unknown"),
        batch(vec![call(
            "effort",
            SET_EFFORT_TOOL,
            json!({"effort":"xhigh"}),
        )]),
        select("denied", "b"),
        answer(),
    ]);
    let registry = Registry {
        a: a.clone(),
        b: Model::new(vec![]),
        fail_b: false,
    };
    let f = Fixture::new(vec![ControlOutcome::Denied], false, None);
    let controls = f.controls(&registry, 0, 4);
    let store = Arc::new(MemoryCheckpointStore::default());
    SessionEngine::new(a.as_ref(), &Runtime::default())
        .with_checkpoint_store(store.clone())
        .run(f.inputs(&controls, 6), &mut History::default())
        .unwrap();
    let cp = saved(store.as_ref(), &f);
    assert_eq!(cp.model, "wire-a");
    assert_eq!(cp.state.spent.control_attempts, 4);
    assert_eq!(
        cp.state
            .transitions
            .iter()
            .map(|t| t.outcome)
            .collect::<Vec<_>>(),
        vec![
            TransitionOutcome::NoOp,
            TransitionOutcome::UnknownModel,
            TransitionOutcome::InvalidArguments,
            TransitionOutcome::Denied
        ]
    );
    assert_eq!(f.seen.lock().unwrap()[0].sequence, 4);
    assert_eq!(cp.context_revision, 0);
    assert!(cp.state.transitions[3].decision_ref.is_some());
}

#[test]
fn client_preparation_failure_and_unsupported_adapter_effort_are_certain_local_refusals() {
    for fail in [true, false] {
        let a = Model::new(vec![
            if fail {
                select("switch", "b")
            } else {
                batch(vec![call(
                    "switch",
                    SELECT_MODEL_TOOL,
                    json!({"model":"b","effort":"high"}),
                )])
            },
            answer(),
        ]);
        let registry = Registry {
            a: a.clone(),
            b: Arc::new(Model {
                default_only: true,
                ..Default::default()
            }),
            fail_b: fail,
        };
        let f = Fixture::new(vec![], false, None);
        let controls = f.controls(&registry, 0, 4);
        let store = Arc::new(MemoryCheckpointStore::default());
        SessionEngine::new(a.as_ref(), &Runtime::default())
            .with_checkpoint_store(store.clone())
            .run(f.inputs(&controls, 3), &mut History::default())
            .unwrap();
        let cp = saved(store.as_ref(), &f);
        assert_eq!(
            cp.state.transitions[0].outcome,
            if fail {
                TransitionOutcome::PreparationFailed
            } else {
                TransitionOutcome::UnsupportedEffort
            }
        );
        assert_eq!(cp.model, "wire-a");
        assert!(f.seen.lock().unwrap().is_empty());
    }
}

#[test]
fn oscillation_is_bounded_and_new_segments_do_not_reset_control_or_model_budgets() {
    let a = Model::new(vec![
        select("ab1", "b"),
        select("ab2", "b"),
        select("exhausted", "b"),
        answer(),
    ]);
    let b = Model::new(vec![select("ba1", "a"), select("ba2", "a")]);
    let registry = Registry {
        a: a.clone(),
        b,
        fail_b: false,
    };
    let f = Fixture::new(vec![ControlOutcome::Admitted; 4], false, None);
    let controls = f.controls(&registry, 0, 4);
    let store = Arc::new(MemoryCheckpointStore::default());
    SessionEngine::new(a.as_ref(), &Runtime::default())
        .with_checkpoint_store(store.clone())
        .run(f.inputs(&controls, 6), &mut History::default())
        .unwrap();
    let cp = saved(store.as_ref(), &f);
    assert_eq!(cp.state.spent.control_attempts, 4);
    assert_eq!(cp.state.spent.model_calls, 6);
    assert_eq!(
        cp.state.transitions[4].outcome,
        TransitionOutcome::AttemptsExhausted
    );
    assert!(
        a.requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .iter()
            .all(|t| t.name != SELECT_MODEL_TOOL && t.name != SET_EFFORT_TOOL)
    );
    assert_eq!(f.seen.lock().unwrap().len(), 4);
}

#[test]
fn effort_setting_propagates_and_omitted_selection_effort_preserves_it() {
    let a = Model::new(vec![
        batch(vec![call(
            "high",
            SET_EFFORT_TOOL,
            json!({"effort":"high"}),
        )]),
        select("switch", "b"),
    ]);
    let b = Model::new(vec![answer()]);
    let registry = Registry {
        a: a.clone(),
        b: b.clone(),
        fail_b: false,
    };
    let f = Fixture::new(vec![ControlOutcome::Admitted; 2], false, None);
    let controls = f.controls(&registry, 0, 4);
    SessionEngine::new(a.as_ref(), &Runtime::default())
        .run(f.inputs(&controls, 4), &mut History::default())
        .unwrap();
    assert_eq!(
        a.requests.lock().unwrap()[0].2.effort(),
        Effort::ProviderDefault
    );
    assert_eq!(a.requests.lock().unwrap()[1].2.effort(), Effort::High);
    assert_eq!(b.requests.lock().unwrap()[0].2.effort(), Effort::High);
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen[1].from.effort, Effort::High);
    assert_eq!(seen[1].to.effort, Effort::High);
}

#[test]
fn direct_mode_has_no_authorizer_or_tools_and_forged_mixed_batches_execute_nothing() {
    let model = Model::new(vec![
        batch(vec![
            call("script", "bash", json!({"script":"echo hi"})),
            call("control", SELECT_MODEL_TOOL, json!({"model":"b"})),
        ]),
        answer(),
    ]);
    let runtime = Runtime::default();
    let mut history = History::default();
    SessionEngine::new(model.as_ref(), &runtime)
        .run(
            SessionBootstrap::new(
                "hi",
                PromptLimits {
                    max_steps: 3,
                    max_capability_calls: 2,
                },
                "direct",
            ),
            &mut history,
        )
        .unwrap();
    assert_eq!(runtime.0.load(Ordering::SeqCst), 0);
    assert!(
        model.requests.lock().unwrap()[0]
            .1
            .iter()
            .all(|t| !matches!(t.name.as_str(), SELECT_MODEL_TOOL | SET_EFFORT_TOOL))
    );
}

struct Cancel(Arc<AtomicBool>);
impl CancellationProbe for Cancel {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
#[test]
fn epoch_change_and_stop_after_admission_halt_without_applying_or_calling_the_target() {
    for stop in [false, true] {
        let a = Model::new(vec![select("switch", "b")]);
        let b = Model::new(vec![]);
        let registry = Registry {
            a: a.clone(),
            b: b.clone(),
            fail_b: false,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = Cancel(cancelled.clone());
        let f = Fixture::new(
            vec![ControlOutcome::Admitted],
            !stop,
            stop.then_some(cancelled),
        );
        let controls = f.controls(&registry, 0, 4);
        let store = Arc::new(MemoryCheckpointStore::default());
        let error = SessionEngine::new(a.as_ref(), &Runtime::default())
            .with_checkpoint_store(store.clone())
            .run(
                f.inputs(&controls, 3).with_cancellation(&cancel),
                &mut History::default(),
            )
            .unwrap_err();
        assert!(if stop {
            matches!(error, PromptError::Cancelled)
        } else {
            matches!(
                error,
                PromptError::Control(ControlError::Authorization(
                    dekopon_broker_protocol::ClientError::SurfaceChanged
                ))
            )
        });
        let cp = saved(store.as_ref(), &f);
        assert_eq!(cp.model, "wire-a");
        assert!(cp.state.control_fenced);
        assert!(b.requests.lock().unwrap().is_empty());
        if !stop {
            // The checkpointed record says *which* client failure fenced the job, not just that
            // one did. A reader of this checkpoint can tell "the broker restarted underneath us"
            // from "something answered the socket with a decision bound to another proposal".
            assert_eq!(
                cp.state.transitions.last().map(|t| t.outcome),
                Some(TransitionOutcome::AuthorizationFailed {
                    cause: ControlFailureKind::Client(ClientErrorKind::SurfaceChanged),
                })
            );
        }
    }
}

#[test]
fn an_unusable_control_surface_reports_every_conflict_in_one_refusal() {
    // Two independent things are wrong: the attempt budget is out of range and the baseline
    // selection is not a candidate. One silent `Configuration` made the operator fix one, restart,
    // and discover the other.
    struct Elsewhere;
    impl ModelRegistry for Elsewhere {
        fn candidates(&self) -> Vec<ControlTarget> {
            vec![ControlTarget {
                model: "elsewhere".parse().unwrap(),
                efforts: vec![Effort::Low],
            }]
        }
        fn prepare(&self, _: &ModelSelection) -> Result<PreparedModel, PreparationError> {
            Err(PreparationError::UnknownModel)
        }
    }
    let f = Fixture::new(vec![], false, None);
    let error = SessionControls::new(
        &Elsewhere,
        selection("a"),
        f.client
            .control_client(f.scope.clone(), f.epoch.clone(), None, 0)
            .unwrap(),
        f.rt.handle().clone(),
        99,
    );
    let Err(error) = error else {
        panic!("an unusable control surface must refuse")
    };
    let message = error.to_string();
    for cause in ["control attempt budget 99", "baseline selection a/"] {
        assert!(message.contains(cause), "{cause:?} missing from {message}");
    }
    assert_eq!(
        ControlFailureKind::of(&error),
        ControlFailureKind::Configuration
    );
}

#[test]
fn select_model_offers_only_efforts_the_configured_targets_carry() {
    // The gateway's mirror must not offer what `controlTargets` rejects: a proposal naming an
    // effort no candidate lists is `target-denied` at the broker while still costing prompt tokens
    // and one of the job's four attempts.
    struct Narrow;
    impl ModelRegistry for Narrow {
        fn candidates(&self) -> Vec<ControlTarget> {
            ["a", "b"]
                .into_iter()
                .map(|model| ControlTarget {
                    model: model.parse().unwrap(),
                    efforts: vec![Effort::Low, Effort::High],
                })
                .collect()
        }
        fn prepare(&self, _: &ModelSelection) -> Result<PreparedModel, PreparationError> {
            Err(PreparationError::UnknownModel)
        }
    }
    let f = Fixture::new(vec![], false, None);
    let controls = SessionControls::new(
        &Narrow,
        ModelSelection {
            model: "a".parse().unwrap(),
            effort: Effort::Low,
        },
        f.client
            .control_client(f.scope.clone(), f.epoch.clone(), None, 0)
            .unwrap(),
        f.rt.handle().clone(),
        4,
    )
    .expect("a narrow but coherent surface builds");
    let current = ModelIdentity {
        configured: Some("a".parse().unwrap()),
        backend: "wire".into(),
        model: "wire-a".into(),
        effort: Effort::Low,
    };
    let model = Model::new(vec![]);
    let tools = controls.tools(&current, model.as_ref(), 0);
    let select = tools
        .iter()
        .find(|tool| tool.name == SELECT_MODEL_TOOL)
        .expect("a second candidate offers select_model");
    let efforts = select.parameters["properties"]["effort"]["enum"].clone();
    assert_eq!(efforts, serde_json::json!(["low", "high"]));
}

#[test]
fn two_different_client_failures_are_two_different_authorization_outcomes() {
    // `AuthorizationFailed` on its own collapsed every one of these into one token, and the
    // `ClientError` behind it was logged nowhere, so a response substitution and an unreachable
    // broker produced byte-identical evidence.
    let binding = TransitionOutcome::AuthorizationFailed {
        cause: ControlFailureKind::of(&ControlError::Authorization(
            dekopon_broker_protocol::ClientError::ControlBinding,
        )),
    };
    let timeout = TransitionOutcome::AuthorizationFailed {
        cause: ControlFailureKind::of(&ControlError::Authorization(
            dekopon_broker_protocol::ClientError::ConnectTimeout,
        )),
    };
    assert_ne!(binding, timeout);
    let rendered = |outcome| serde_json::to_string(&outcome).expect("outcome serializes");
    assert!(rendered(binding).contains("control-binding"), "{binding:?}");
    assert!(rendered(timeout).contains("connect-timeout"), "{timeout:?}");
    // And the interrupted case is neither: nothing reached the broker at all.
    assert_ne!(
        binding,
        TransitionOutcome::AuthorizationFailed {
            cause: ControlFailureKind::Interrupted,
        }
    );
}

struct FailApplied(MemoryCheckpointStore);
impl CheckpointStore for FailApplied {
    fn load(&self, j: &str) -> Result<Checkpoint, CheckpointError> {
        self.0.load(j)
    }
    fn acquire(&self, j: &str, n: bool) -> Result<String, CheckpointError> {
        self.0.acquire(j, n)
    }
    fn compare_and_save(
        &self,
        l: &str,
        e: u64,
        c: &Checkpoint,
    ) -> Result<SaveReceipt, CheckpointError> {
        if c.state
            .transitions
            .last()
            .is_some_and(|t| t.outcome == TransitionOutcome::Applied)
        {
            Err(CheckpointError::Conflict)
        } else {
            self.0.compare_and_save(l, e, c)
        }
    }
    fn release(&self, j: &str, l: &str, f: bool) {
        self.0.release(j, l, f)
    }
}
#[test]
fn failed_post_switch_checkpoint_retains_live_transition_and_prevents_target_inference() {
    let a = Model::new(vec![select("switch", "b")]);
    let b = Model::new(vec![]);
    let registry = Registry {
        a: a.clone(),
        b: b.clone(),
        fail_b: false,
    };
    let f = Fixture::new(vec![ControlOutcome::Admitted], false, None);
    let controls = f.controls(&registry, 0, 4);
    let store = Arc::new(FailApplied(MemoryCheckpointStore::default()));
    let error = SessionEngine::new(a.as_ref(), &Runtime::default())
        .with_checkpoint_store(store.clone())
        .run(f.inputs(&controls, 3), &mut History::default())
        .unwrap_err();
    let PromptError::Interrupted { checkpoint, .. } = error else {
        panic!("expected live checkpoint")
    };
    assert_eq!(checkpoint.model, "wire-b");
    assert_eq!(
        checkpoint.state.transitions[0].outcome,
        TransitionOutcome::Applied
    );
    assert_eq!(checkpoint.state.spent.model_calls, 1);
    assert!(b.requests.lock().unwrap().is_empty());
    assert_eq!(
        store.load(f.scope.job.as_str()).unwrap_err(),
        CheckpointError::Fenced
    );
}

#[test]
fn restored_noninitial_selection_requires_fresh_baseline_authorization_without_recounting_calls() {
    let a = Model::new(vec![select("switch", "b")]);
    let b = Model::new(vec![answer()]);
    let registry = Registry {
        a: a.clone(),
        b: b.clone(),
        fail_b: false,
    };
    let f = Fixture::new(vec![ControlOutcome::Admitted; 2], false, None);
    let controls = f.controls(&registry, 0, 4);
    let store = Arc::new(MemoryCheckpointStore::default());
    let runtime = Runtime::default();
    let engine = SessionEngine::new(a.as_ref(), &runtime).with_checkpoint_store(store.clone());
    let ledger = crate::accounting::JobAccounting::default();
    let first = engine
        .run(
            f.inputs(&controls, 3).with_accounting(&ledger),
            &mut History::default(),
        )
        .unwrap();
    let controls = f.controls(&registry, 1, 4);
    let resumed = engine
        .run(
            f.inputs(&controls, 3)
                .with_accounting(&ledger)
                .with_resume(&first.job),
            &mut History::default(),
        )
        .unwrap();
    assert_eq!(resumed.model_turns, 2);
    assert_eq!(first.job, resumed.job);
    assert_eq!(b.requests.lock().unwrap().len(), 1);
    let cp = saved(store.as_ref(), &f);
    assert_eq!(cp.state.spent.control_attempts, 2);
    assert_eq!(cp.state.accounting.calls.len(), 2);
    assert_eq!(
        cp.state.accounting.segment, 1,
        "restore admission is not another spend segment"
    );
    assert_eq!(
        cp.state.accounting.totals().cumulative.input.known,
        Some(10)
    );
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen[1].from, selection("a"));
    assert_eq!(seen[1].to, selection("b"));
    assert_eq!(seen[1].sequence, 2);
}

struct EvidenceInvoker;
impl dekopon_shell::CapabilityInvoker for EvidenceInvoker {
    fn granted(&self) -> Vec<String> {
        vec!["evidence.read".into()]
    }
    fn describe(&self, c: &str) -> Option<dekopon_shell::CapabilityDescription> {
        Some(dekopon_shell::CapabilityDescription {
            capability: c.into(),
            description: "fixture".into(),
            input_schema: json!({"type":"object"}),
        })
    }
    fn invoke(
        &self,
        _: &str,
        _: serde_json::Value,
        _: Option<dekopon_core::SecretUseProposal>,
    ) -> dekopon_shell::CapabilityCallResult {
        dekopon_shell::CapabilityCallResult::Succeeded(json!({"observed":"evidence-retained"}))
    }
}
struct FailingImage(AtomicU32);
impl dekopon_model::image::ImageGenerator for FailingImage {
    fn generate(
        &self,
        _: &str,
        recorder: &dyn dekopon_model::usage::AttemptRecorder,
    ) -> Result<dekopon_model::image::GeneratedImage, dekopon_model::image::ImageGenerationError>
    {
        recorder.begin(dekopon_model::usage::AttemptKind::Adapter)?;
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(dekopon_model::image::ImageGenerationError::InvalidImage)
    }
}
#[test]
fn execution_evidence_and_image_attempt_flag_survive_a_switch_and_failed_final_inference() {
    let a = Model::new(vec![
        batch(vec![call(
            "work",
            "bash",
            json!({"script":"cap evidence.read"}),
        )]),
        batch(vec![call(
            "image",
            "generate_image",
            json!({"prompt":"fixture"}),
        )]),
        select("switch", "b"),
    ]);
    let b = Model::new(vec![batch(vec![call(
        "again",
        "generate_image",
        json!({"prompt":"fixture"}),
    )])]);
    b.turns
        .lock()
        .unwrap()
        .push_back(Err(ModelError::Request("fixture failure".into())));
    let registry = Registry {
        a: a.clone(),
        b: b.clone(),
        fail_b: false,
    };
    let f = Fixture::new(vec![ControlOutcome::Admitted], false, None);
    let controls = f.controls(&registry, 0, 4);
    let runtime = crate::runtime::ShellRuntime {
        invoker: EvidenceInvoker,
        limits: dekopon_shell::Limits::default(),
        curl_capability: None,
    };
    let image = FailingImage(AtomicU32::new(0));
    let output = crate::tools::GeneratedImageOutput::default();
    let store = Arc::new(MemoryCheckpointStore::default());
    let mut history = History::default();
    let error = SessionEngine::new(a.as_ref(), &runtime)
        .with_checkpoint_store(store.clone())
        .run(
            f.inputs(&controls, 6)
                .with_image_generation(&image, &output),
            &mut history,
        )
        .unwrap_err();
    assert!(matches!(error, PromptError::Model(ModelError::Request(_))));
    let cp = saved(store.as_ref(), &f);
    assert_eq!(cp.state.spent.capability_invocations, 1);
    assert_eq!(cp.state.spent.script_calls, 1);
    assert!(cp.state.image_generation_attempted);
    assert_eq!(image.0.load(Ordering::SeqCst), 1);
    assert_eq!(cp.record.executions.len(), 1);
    assert_eq!(
        cp.record.executions[0].outcome,
        crate::history::ExecutionOutcome::Succeeded
    );
    assert_eq!(
        cp.record.executions[0].provenance,
        crate::history::ExecutionProvenance::DirectReadOnly
    );
    assert!(
        serde_json::to_string(&cp.record)
            .unwrap()
            .contains("evidence-retained")
    );
    assert_eq!(cp.state.spent.model_calls, 5);
    assert_eq!(
        history.turns().last().unwrap().executions,
        cp.record.executions
    );
}

#[test]
fn control_arguments_never_accept_endpoint_credentials_or_arbitrary_effort() {
    let identity = ModelIdentity {
        configured: Some("a".parse().unwrap()),
        backend: "fixture".into(),
        model: "wire-a".into(),
        effort: Effort::Low,
    };
    for args in [
        json!({"model":"https://bad"}),
        json!({"model":"b","endpoint":"https://bad"}),
        json!({"model":"b","apiKey":"not-a-credential"}),
        json!({"model":"b","effort":"maximum"}),
        json!({"model":"b","effort":null}),
        json!({"model":"b","agent":"other"}),
    ] {
        assert_eq!(
            parse(&call("id", SELECT_MODEL_TOOL, args), &identity),
            Err(TransitionOutcome::InvalidArguments)
        );
    }
    assert_eq!(
        parse(
            &call("id", SELECT_MODEL_TOOL, json!({"model":"b"})),
            &identity
        )
        .unwrap()
        .effort,
        Effort::Low
    );
}

#[test]
fn decline_precedence_refuses_controls_without_authorization_or_other_tools() {
    let a = Model::new(vec![batch(vec![
        call("decline", "decline_chat_reply", json!({})),
        call("switch", SELECT_MODEL_TOOL, json!({"model":"b"})),
        call("script", "bash", json!({"script":"echo hi"})),
    ])]);
    let registry = Registry {
        a: a.clone(),
        b: Model::new(vec![]),
        fail_b: false,
    };
    let f = Fixture::new(vec![], false, None);
    let controls = f.controls(&registry, 0, 4);
    let store = Arc::new(MemoryCheckpointStore::default());
    let runtime = Runtime::default();
    let exit = SessionEngine::new(a.as_ref(), &runtime)
        .with_checkpoint_store(store.clone())
        .run(
            f.inputs(&controls, 2).with_optional_reply(),
            &mut History::default(),
        )
        .unwrap();
    assert_eq!(exit.disposition, crate::session::ReplyDisposition::Suppress);
    assert_eq!(runtime.0.load(Ordering::SeqCst), 0);
    assert!(f.seen.lock().unwrap().is_empty());
    assert_eq!(
        saved(store.as_ref(), &f).state.transitions[0].outcome,
        TransitionOutcome::BatchRefused
    );
}
