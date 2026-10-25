use std::time::SystemTime;

use dekopon_agent::wake::{WAKE_TOOL_NAME, WakeId, WakeRefusal};
use dekopon_broker_protocol::Trigger;
use dekopon_process::ExitCode;
use dekopon_shell::ScriptOutcome;

use super::*;
use crate::{
    config::{ResolvedWakes, WakeBounds},
    wake::{Anchor, Probe, Resolved, Verdict, WakeStore, run_tick, store::Schedule},
};

struct NoProviders;

impl dekopon_shell::CapabilityInvoker for NoProviders {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }

    fn invoke(
        &self,
        _: dekopon_shell::CommandProposal,
        _streams: dekopon_shell::Streams,
        _tree: &dekopon_process::TreeContext,
    ) -> dekopon_shell::CapabilityCallResult {
        dekopon_shell::secret_use_unsupported()
    }
}

pub(super) fn wake_store(directory: &Path) -> Arc<WakeStore> {
    Arc::new(
        WakeStore::open(&ResolvedWakes {
            path: directory.join("wakes").join("wakes.jsonl"),
            bounds: WakeBounds {
                max_per_subject: 4,
                min_interval: Duration::from_secs(1),
                max_horizon: Duration::from_secs(3600),
            },
        })
        .expect("the wake store opens"),
    )
}

pub(super) fn slack_message(text: &str) -> InboundMessage {
    let mut message = message(text);
    message.transport_kind = dekopon_broker_protocol::ChatTransportKind::Slack;
    message.reply = ReplyTarget::Slack {
        channel: "d0123abc".to_owned(),
        thread_ts: None,
    };
    message
}

fn anchor() -> Anchor {
    Anchor::from_inbound(&slack_message("anchor"), &route(model_config()).agent)
        .expect("anchorable")
}

fn wake_route() -> crate::routes::BoundRoute {
    let mut route = route(model_config());
    route.wakes = true;
    route
}

fn runner_with_wakes(
    broker: ResolvedBroker,
    models: Arc<ModelScript>,
    store: &Arc<WakeStore>,
) -> Arc<SessionRunner> {
    let mut runner = Arc::into_inner(runner(broker, models, 4)).expect("sole owner");
    runner.wakes = Some(Arc::clone(store));
    Arc::new(runner)
}

pub(super) fn wake_call(arguments: &Value) -> AssistantTurn {
    AssistantTurn::new(
        None,
        vec![ModelToolCall {
            id: "wake-call".into(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: WAKE_TOOL_NAME.to_owned(),
                arguments: arguments.to_string(),
            },
        }],
        None,
    )
}

fn outcome(exit: u8, output: &str, truncated: bool) -> ScriptOutcome {
    ScriptOutcome {
        output: output.to_owned(),
        exit_code: ExitCode::from(exit),
        truncated,
        capability_calls: 0,
        steps: 0,
    }
}

fn once(store: &WakeStore, note: &str, now: SystemTime) -> WakeId {
    store
        .register(
            anchor(),
            note.to_owned(),
            now,
            Schedule::Once {
                after: Duration::from_secs(1),
            },
        )
        .expect("registered")
        .id
}

fn triggers(observed: &mut mpsc::UnboundedReceiver<RequestEnvelope>) -> Vec<Trigger> {
    let mut triggers = Vec::new();
    while let Ok(request) = observed.try_recv() {
        if let BrokerRequest::Capabilities {
            attestation: Some(claim),
        } = request.request
        {
            triggers.push(claim.scope.expect("a chat scope").trigger);
        }
    }
    triggers
}

#[test]
fn only_exit_zero_fires_only_exit_one_waits_and_oversized_output_is_a_failure() {
    assert_eq!(
        Verdict::from(outcome(0, "green", false)),
        Verdict::Fire {
            output: "green".to_owned()
        }
    );
    assert_eq!(
        Verdict::from(outcome(1, "red", false)),
        Verdict::Wait {
            output: "red".to_owned()
        }
    );
    for exit in [2, 124, 126, 127] {
        assert!(matches!(
            Verdict::from(outcome(exit, "", false)),
            Verdict::Failed { exit: code, .. } if code.get() == exit
        ));
    }
    assert!(matches!(
        Verdict::from(outcome(0, "green", true)),
        Verdict::Failed { .. }
    ));
    assert!(matches!(
        Verdict::from(outcome(1, &"x".repeat(9 * 1024), false)),
        Verdict::Failed { .. }
    ));
}

#[test]
fn a_baseline_never_fires_and_a_broken_probe_is_never_stored() {
    let limits = dekopon_process::Limits::default();
    assert!(
        Probe::baseline("echo green; exit 0".to_owned(), &NoProviders, limits).is_ok(),
        "an already-true condition is stored, not fired"
    );
    assert!(matches!(
        Probe::baseline("exit 2".to_owned(), &NoProviders, limits),
        Err(WakeRefusal::Broken { exit: 2, .. })
    ));
    assert!(matches!(
        Probe::baseline("no-such-word".to_owned(), &NoProviders, limits),
        Err(WakeRefusal::Broken { exit: 127, .. })
    ));
}

struct ChecksStatus(u8);

impl ChecksStatus {
    fn parse_command(
        &self,
        word: &str,
        argv: &[String],
        _stdin_piped: bool,
    ) -> Option<Result<dekopon_shell::CommandProposal, dekopon_shell::CommandRun>> {
        assert_eq!(word, "gh");
        assert_eq!(argv, ["pr", "checks", "-R", "owner/repo", "42"]);
        Some(Err(dekopon_shell::CommandRun::Rendered {
            stdout: "check table must not enter the probe".to_owned(),
            stderr: String::new(),
            status: self.0,
        }))
    }
}

impl dekopon_shell::CapabilityInvoker for ChecksStatus {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }

    fn command_words(&self) -> Vec<String> {
        vec!["gh".to_owned()]
    }

    fn run(
        &self,
        call: dekopon_shell::RunCall,
        streams: dekopon_shell::Streams,
        tree: &dekopon_process::TreeContext,
    ) -> dekopon_shell::RunResult {
        let argv = call
            .argv
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        match self.parse_command(&call.word, &argv, streams.stdin.is_some()) {
            Some(Ok(proposal)) => {
                let capability = proposal.capability.clone();
                let input = proposal.input.clone();
                let secret_use = proposal.secret_use.clone();
                if !self.is_granted(&capability) {
                    return dekopon_shell::render_command(
                        streams,
                        "",
                        format!(
                            "dekopon-shell: {}: requires capability {}, which is not granted in this session\n",
                            call.word, capability
                        ),
                        127,
                    );
                }
                let result = self.invoke(proposal, streams, tree);
                dekopon_shell::RunResult::Invoked {
                    capability,
                    input,
                    secret_use,
                    result,
                }
            }
            Some(Err(rendered)) => dekopon_shell::render_run(call, Some(rendered), streams),
            None => dekopon_shell::render_run(call, None, streams),
        }
    }

    fn invoke(
        &self,
        _: dekopon_shell::CommandProposal,
        _streams: dekopon_shell::Streams,
        _tree: &dekopon_process::TreeContext,
    ) -> dekopon_shell::CapabilityCallResult {
        dekopon_shell::secret_use_unsupported()
    }
}

#[test]
fn checks_recipe_waits_on_pending_fires_with_one_word_and_stops_on_errors() {
    let script = "gh pr checks -R owner/repo 42 > /dev/null; s=$?; case $s in 0) echo pass; exit 0;; 1) echo fail; exit 0;; 8) exit 1;; *) exit $s;; esac";
    let limits = dekopon_process::Limits::default();
    for (status, word) in [(0, "pass"), (1, "fail")] {
        let probe = Probe::baseline(script.to_owned(), &ChecksStatus(8), limits).expect("wait");
        assert_eq!(
            probe.tick(&ChecksStatus(status), limits),
            Verdict::Fire {
                output: word.to_owned()
            }
        );
    }
    let probe = Probe::baseline(script.to_owned(), &ChecksStatus(8), limits).expect("wait");
    assert_eq!(
        probe.tick(&ChecksStatus(8), limits),
        Verdict::Wait {
            output: String::new()
        }
    );
    let directory = temporary();
    let store = wake_store(directory.path());
    let now = SystemTime::now();
    store
        .register(
            anchor(),
            "check the PR".to_owned(),
            now,
            Schedule::Watch {
                probe,
                every: Duration::from_secs(60),
                until: Duration::from_secs(600),
            },
        )
        .expect("registered");
    let due = now + Duration::from_secs(61);
    let tick = store
        .take_due(due)
        .expect("readable")
        .ticks
        .pop()
        .expect("due");
    let verdict = tick.run(&ChecksStatus(2), limits);
    assert!(matches!(&verdict, Verdict::Failed { exit, .. } if exit.get() == 2));
    let Resolved::Fired(fired) = tick.resolve(&store, verdict, due).expect("resolved") else {
        panic!("a failed check must retire the watch")
    };
    let inbound = fired.into_inbound();
    assert!(inbound.text.contains("stopped because its probe exited 2"));
    assert!(matches!(&inbound.message_id, MessageId::Wake { notice, .. }
        if notice.starts_with("check the PR") && !notice.ends_with("\n\npass") && !notice.ends_with("\n\nfail")));
    assert!(!inbound.text.contains("[gateway: the probe printed:]\npass"));
    assert!(!inbound.text.contains("[gateway: the probe printed:]\nfail"));
    assert!(store.list(anchor().subject(), due).is_empty());
}

#[test]
fn a_local_chat_cannot_anchor_a_wake() {
    assert!(Anchor::from_inbound(&message("later"), &route(model_config()).agent).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watch_interval_longer_than_its_run_is_refused_before_any_probe() {
    use dekopon_agent::wake::{WakeRegistrar, WakeRequest};
    let directory = temporary();
    let store = wake_store(directory.path());
    let wakes = crate::wake::SessionWakes::new(
        anchor(),
        Arc::clone(&store),
        ResolvedBroker {
            socket_path: directory.path().join("absent.sock"),
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        tokio::runtime::Handle::current(),
        dekopon_process::Limits::default(),
        None,
    );
    let refused = tokio::task::spawn_blocking(move || {
        wakes.schedule(WakeRequest::Watch {
            note: "check".to_owned(),
            script: "exit 1".to_owned(),
            every: Duration::from_secs(u64::MAX),
            until: Duration::from_secs(60),
        })
    })
    .await
    .expect("joined");
    assert!(matches!(refused, Err(WakeRefusal::Interval { .. })));
}

#[test]
fn a_due_watch_is_leased_once_and_a_cancel_mid_tick_never_fires() {
    let directory = temporary();
    let store = wake_store(directory.path());
    let now = SystemTime::now();
    let probe = Probe::baseline(
        "echo waiting; exit 1".to_owned(),
        &NoProviders,
        dekopon_process::Limits::default(),
    )
    .expect("baseline");
    let id = store
        .register(
            anchor(),
            "check".to_owned(),
            now,
            Schedule::Watch {
                probe,
                every: Duration::from_secs(60),
                until: Duration::from_secs(600),
            },
        )
        .expect("registered")
        .id;

    let due = now + Duration::from_secs(61);
    let mut first = store.take_due(due).expect("readable");
    assert_eq!(first.ticks.len(), 1);
    assert!(
        store.take_due(due).expect("readable").ticks.is_empty(),
        "a leased watch is not leased again while its tick runs"
    );

    store.cancel(anchor().subject(), id).expect("cancelled");
    let tick = first.ticks.pop().expect("tick");
    let resolved = tick
        .resolve(
            &store,
            Verdict::Fire {
                output: "green".to_owned(),
            },
            due,
        )
        .expect("resolved");
    assert!(matches!(resolved, Resolved::Gone));
}

#[test]
fn a_one_shot_fires_once_and_only_after_its_row_is_gone() {
    let directory = temporary();
    let store = wake_store(directory.path());
    let now = SystemTime::now();
    once(&store, "call mom", now);

    assert!(store.take_due(now).expect("readable").fired.is_empty());
    let due = now + Duration::from_secs(2);
    let fired = store.take_due(due).expect("readable").fired;
    assert_eq!(fired.len(), 1);
    assert!(store.take_due(due).expect("readable").fired.is_empty());
    assert!(store.list(anchor().subject(), due).is_empty());
    assert!(
        WakeStore::open(&ResolvedWakes {
            path: directory.path().join("wakes").join("wakes.jsonl"),
            bounds: store.bounds(),
        })
        .expect("reopens")
        .next_at()
        .is_none(),
        "the retired row is gone from disk"
    );
}

#[test]
fn a_failed_write_fires_nothing_and_leaves_the_wake_pending() {
    let directory = temporary();
    let store = wake_store(directory.path());
    let now = SystemTime::now();
    once(&store, "call mom", now);
    let folder = directory.path().join("wakes");
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o500)).expect("read-only");

    let result = store.take_due(now + Duration::from_secs(2));

    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).expect("writable");
    assert!(matches!(
        result,
        Err(crate::wake::WakeStoreError::Io { .. })
    ));
    assert_eq!(store.list(anchor().subject(), now).len(), 1);
}

#[test]
fn pending_wakes_survive_a_restart_and_a_corrupt_store_refuses_to_start() {
    let directory = temporary();
    let now = SystemTime::now();
    {
        let store = wake_store(directory.path());
        once(&store, "first", now);
        once(&store, "second", now);
    }
    let store = wake_store(directory.path());
    let pending = store.list(anchor().subject(), now);
    assert_eq!(
        pending
            .iter()
            .map(|wake| wake.note.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(once(&store, "third", now), WakeId(3));

    let path = directory.path().join("wakes").join("wakes.jsonl");
    let mut text = fs::read_to_string(&path).expect("store text");
    text.push_str("{\"id\":\n");
    fs::write(&path, text).expect("corrupted");
    assert!(matches!(
        WakeStore::open(&ResolvedWakes {
            path,
            bounds: store.bounds(),
        }),
        Err(crate::wake::WakeStoreError::Corrupt { line: 4 })
    ));
}

#[test]
fn a_pending_watch_restarts_with_its_probe_and_original_chat_anchor() {
    let directory = temporary();
    let now = SystemTime::now();
    {
        let store = wake_store(directory.path());
        store
            .register(
                anchor(),
                "check the PR".to_owned(),
                now,
                Schedule::Watch {
                    probe: Probe::baseline(
                        "echo pass; exit 0".to_owned(),
                        &NoProviders,
                        dekopon_process::Limits::default(),
                    )
                    .expect("baseline"),
                    every: Duration::from_secs(60),
                    until: Duration::from_secs(600),
                },
            )
            .expect("registered");
    }
    let store = wake_store(directory.path());
    let due = now + Duration::from_secs(61);
    let tick = store
        .take_due(due)
        .expect("readable")
        .ticks
        .pop()
        .expect("restored watch");
    let verdict = tick.run(&NoProviders, dekopon_process::Limits::default());
    assert_eq!(
        verdict,
        Verdict::Fire {
            output: "pass".to_owned()
        }
    );
    let fired = tick.resolve(&store, verdict, due).expect("resolved");
    let Resolved::Fired(fired) = fired else {
        panic!("a terminal check must fire")
    };
    let inbound = fired.into_inbound();
    assert_eq!(inbound.transport, anchor().transport().to_string());
    assert_eq!(inbound.subject, anchor().subject().clone());
    assert_eq!(inbound.conversation, anchor().conversation().clone());
    assert_eq!(inbound.reply, slack_message("anchor").reply);
    assert!(inbound.text.contains("pass"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watch_carries_prev_between_read_only_ticks_and_fires_once() {
    let directory = temporary();
    let listing = || {
        ResponseEnvelope::capabilities(
            vec![capability("cli-probe.upper")],
            Vec::new(),
            BTreeMap::new(),
        )
    };
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![listing(), listing(), listing(), listing()],
    )
    .await;
    let script = r#"if [ -z "$PREV" ]; then echo one; exit 1; fi
if [ "$PREV" = one ]; then echo two; exit 1; fi
echo "after $PREV""#;
    let models = ModelScript::new([
        wake_call(&json!({
            "action": "watch",
            "note": "tell them it is green",
            "script": script,
            "everySeconds": 60,
            "forSeconds": 3600,
        })),
        answer("watching"),
    ]);
    let store = wake_store(directory.path());
    let runner = runner_with_wakes(broker, Arc::clone(&models), &store);
    let driver = Arc::new(RecordingDriver::default());

    run_session(
        Arc::clone(&runner),
        wake_route(),
        slack_message("tell me when it is green"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert!(models.tool_names(0).contains(&WAKE_TOOL_NAME.to_owned()));
    let receipt = tool_message(&models, 1);
    assert!(receipt.contains("last output:\none"), "{receipt}");

    let mut now = SystemTime::now();
    let mut fired = Vec::new();
    for _ in 0..2 {
        now += Duration::from_secs(61);
        let tick = store
            .take_due(now)
            .expect("readable")
            .ticks
            .pop()
            .expect("the watch is due");
        let (store, broker) = (Arc::clone(&store), runner.broker.clone());
        let runtime = tokio::runtime::Handle::current();
        fired.extend(
            tokio::task::spawn_blocking(move || {
                run_tick(
                    tick,
                    &store,
                    &broker,
                    &runtime,
                    Some(dekopon_process::Limits::default()),
                    None,
                )
            })
            .await
            .expect("tick joined"),
        );
    }

    assert_eq!(fired.len(), 1, "the first tick waits and the second fires");
    let text = fired.pop().expect("fired").into_inbound().text;
    assert!(text.contains("tell them it is green"), "{text}");
    assert!(text.contains("after two"), "{text}");
    assert!(store.list(anchor().subject(), now).is_empty());
    assert_eq!(
        triggers(&mut observed),
        [
            Trigger::Message,
            Trigger::Probe,
            Trigger::Probe,
            Trigger::Probe
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fired_wake_answers_in_its_conversation_attested_as_a_wake() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![ResponseEnvelope::capabilities(
            vec![capability("cli-probe.upper")],
            Vec::new(),
            BTreeMap::new(),
        )],
    )
    .await;
    let store = wake_store(directory.path());
    let now = SystemTime::now();
    once(&store, "check the PR", now);
    let fired = store
        .take_due(now + Duration::from_secs(2))
        .expect("readable")
        .fired
        .pop()
        .expect("due");
    let models = ModelScript::new([answer("it merged")]);
    let driver = Arc::new(RecordingDriver::default());

    run_session(
        runner_with_wakes(broker, Arc::clone(&models), &store),
        wake_route(),
        fired.into_inbound(),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;

    assert_eq!(driver.replies(), ["it merged"]);
    let (_, prompt) = models.prompt(0).pop().expect("a user prompt");
    assert!(
        prompt.starts_with("[gateway: unmapped participant]\n[gateway: wake 1 fired."),
        "{prompt}"
    );
    assert!(prompt.contains("check the PR"), "{prompt}");
    assert_eq!(triggers(&mut observed), [Trigger::Wake]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fired_wake_with_no_route_sends_only_the_orphan_notice() {
    let directory = temporary();
    let store = wake_store(directory.path());
    once(
        &store,
        "check the PR",
        SystemTime::now() - Duration::from_secs(2),
    );
    let driver = Arc::new(RecordingDriver::default());
    let drivers: BTreeMap<String, Arc<dyn ChatDriver>> =
        BTreeMap::from([("dev".to_owned(), Arc::clone(&driver) as Arc<dyn ChatDriver>)]);
    let runner = runner_with_wakes(
        ResolvedBroker {
            socket_path: directory.path().join("absent.sock"),
            server_uid: crate::current_uid(),
            frame: FrameLimits::default(),
        },
        ModelScript::forbidden(),
        &store,
    );
    let (_sender, receiver) = mpsc::channel(4);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(crate::serve(
        runner,
        Arc::new(crate::routes::RoutingTable::default()),
        Arc::new(BTreeMap::new()),
        Arc::new(drivers),
        Arc::new(Vec::new()),
        receiver,
        async move {
            shutdown_rx.await.expect("shutdown signal");
        },
        Duration::from_secs(1),
        crate::collection::Collector::new(&[], 4),
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if !driver.replies().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the due wake reaches its original chat");
    shutdown_tx.send(()).expect("serving shutdown receiver");
    assert_eq!(
        serving.await.expect("serving joins"),
        crate::ServeOutcome::Shutdown
    );
    assert_eq!(driver.replies(), ["check the PR"]);
    assert!(store.list(anchor().subject(), SystemTime::now()).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wake_queues_until_the_running_conversation_finishes() {
    let directory = temporary();
    let (broker, mut observed) = stub_broker(
        directory.path(),
        vec![
            ResponseEnvelope::capabilities(
                vec![capability("cli-probe.upper")],
                Vec::new(),
                BTreeMap::new(),
            ),
            ResponseEnvelope::capabilities(
                vec![capability("cli-probe.upper")],
                Vec::new(),
                BTreeMap::new(),
            ),
        ],
    )
    .await;
    let store = wake_store(directory.path());
    let now = SystemTime::now();
    once(&store, "stop", now);
    let inbound = store
        .take_due(now + Duration::from_secs(2))
        .expect("readable")
        .fired
        .pop()
        .expect("due")
        .into_inbound();
    let models = BlockedModel::new("finished");
    let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 4);
    let driver = Arc::new(RecordingDriver::default());
    let held = tokio::spawn(run_session(
        Arc::clone(&runner),
        wake_route(),
        slack_message("first"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    ));
    models.wait_until_entered().await;

    run_session(
        Arc::clone(&runner),
        wake_route(),
        inbound,
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;

    assert!(
        driver.replies().is_empty(),
        "queueing sends no note-only reply"
    );
    models.release();
    held.await.expect("holder and follow-up finish");
    assert_eq!(driver.replies(), ["finished", "finished"]);
    assert_eq!(triggers(&mut observed), [Trigger::Message, Trigger::Wake]);
}
