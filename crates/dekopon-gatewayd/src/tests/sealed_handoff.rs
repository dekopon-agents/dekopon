use super::*;
use crate::{
    collection::Dispositions,
    config::Steering,
    routes::BoundRoute,
    session::{
        Admit, MAILBOX_CAPACITY, MAX_INTERRUPTS, SessionAdmission, SessionCancellation, Taken,
    },
};
use dekopon_agent::prompt::{
    CancellationProbe, Handoff, History, PromptError, PromptLimits, ScriptRuntime, Seal,
    SessionInputs, Steer, SteerSource, run_prompt_session,
};
use std::sync::Barrier;

fn admit(gate: &SessionGate, route: &BoundRoute, message: InboundMessage) -> Admit {
    let receipts = Dispositions(message.constituents.clone());
    gate.admit(route, message, receipts)
}

fn held(gate: &SessionGate, route: &BoundRoute) -> SessionAdmission {
    let Admit::Admitted(admission, _, receipts) = admit(gate, route, message("first")) else {
        panic!("first message must be admitted");
    };
    receipts.finish(crate::session::MessageOutcome::Answered);
    admission
}

fn key() -> (String, String) {
    (message("").transport, message("").conversation.key())
}

fn native(subject: &str, id: &str, text: &str) -> InboundMessage {
    InboundMessage {
        message_id: MessageId::Native(id.to_owned()),
        ..message_from(subject, text)
    }
}

struct GateMailbox {
    gate: SessionGate,
    cancellation: SessionCancellation,
}

fn person(message: InboundMessage) -> Steer {
    Steer::Person {
        prompt: message.text.clone(),
        recorded: message.text,
    }
}

impl SteerSource for GateMailbox {
    fn take(&self) -> Vec<Steer> {
        self.gate
            .take_steers(&key())
            .into_iter()
            .map(person)
            .collect()
    }

    fn seal_or_take(&self) -> Handoff {
        match self.gate.seal_or_take(&key(), &self.cancellation) {
            Taken::Steers(steers) => Handoff::Steers(steers.into_iter().map(person).collect()),
            Taken::Sealed(seal) => Handoff::Sealed(seal),
        }
    }

    fn seal(&self) -> Seal {
        self.gate.seal(&key(), &self.cancellation)
    }
}

struct SteeringTool {
    gate: SessionGate,
    route: BoundRoute,
}

impl ScriptRuntime for SteeringTool {
    fn run_script(&self, _script: &str) -> dekopon_shell::ScriptOutcome {
        assert!(matches!(
            admit(&self.gate, &self.route, native(SUBJECT, "older", "older")),
            Admit::Steered(_)
        ));
        dekopon_shell::ScriptOutcome {
            output: "ok".to_owned(),
            exit_code: 0.into(),
            truncated: false,
            capability_calls: 0,
            steps: 1,
        }
    }

    fn capability_calls_used(&self) -> u32 {
        0
    }
}

#[test]
fn a_steer_during_the_capped_last_steps_tools_runs_next_before_newer_input() {
    let gate = SessionGate::new(1);
    let mut route = route(model_config());
    route.steering = Steering::Boundary;
    let admission = held(&gate, &route);
    assert!(matches!(
        admit(&gate, &route, native("tel.999", "other", "other")),
        Admit::Queued
    ));
    let script = ModelScript::new([script_call("echo")]);
    let mailbox = GateMailbox {
        gate: gate.clone(),
        cancellation: admission.cancellation(),
    };
    let error = run_prompt_session(
        &ScriptedModel(script),
        &SteeringTool {
            gate: gate.clone(),
            route: route.clone(),
        },
        SessionInputs::new(
            "first",
            PromptLimits {
                max_steps: 1,
                max_capability_calls: 4,
            },
        )
        .with_steering(&mailbox),
        &mut History::default(),
    )
    .expect_err("the capped step ends without an answer");
    assert!(matches!(error, PromptError::MaxSteps { maximum: 1 }));
    assert_eq!(
        mailbox.seal(),
        Seal::Sealed { leftover: 1 },
        "the cap sealed with the steer unread"
    );
    assert!(matches!(
        admit(&gate, &route, native(SUBJECT, "newer", "newer")),
        Admit::Queued
    ));

    let (admission, leftover) = admission.next_or_release().expect("the leftover runs");
    assert_eq!(leftover.message.subject.canonical(), SUBJECT);
    assert!(leftover.message.text.contains("older"));
    assert!(!leftover.message.text.contains("newer"));
    let (admission, other) = admission.next_or_release().expect("queued sender");
    assert_eq!(other.message.text, "other");
    let (admission, newer) = admission.next_or_release().expect("newer input");
    assert_eq!(newer.message.text, "newer");
    assert!(admission.next_or_release().is_none());
}

#[test]
fn a_steer_racing_the_seal_is_staged_or_follows_and_is_never_stranded() {
    let route = route(model_config());
    for round in 0..200 {
        let gate = SessionGate::new(1);
        let admission = held(&gate, &route);
        let cancellation = admission.cancellation();
        let barrier = Barrier::new(2);
        let (taken, admitted) = std::thread::scope(|scope| {
            let sealing = scope.spawn(|| {
                barrier.wait();
                if round % 2 == 0 {
                    gate.seal_or_take(&key(), &cancellation)
                } else {
                    Taken::Sealed(gate.seal(&key(), &cancellation))
                }
            });
            barrier.wait();
            let admitted = admit(&gate, &route, message("race"));
            (sealing.join().expect("the seal never panics"), admitted)
        });
        match (taken, admitted) {
            (Taken::Steers(steers), Admit::Steered(_)) => {
                assert_eq!(steers.len(), 1);
                assert!(admission.next_or_release().is_none());
            }
            (Taken::Sealed(Seal::Sealed { leftover: 1 }), Admit::Steered(_))
            | (Taken::Sealed(Seal::Sealed { leftover: 0 }), Admit::Queued) => {
                let (admission, next) = admission.next_or_release().expect("the steer follows");
                assert!(next.message.text.contains("race"));
                assert!(admission.next_or_release().is_none());
            }
            (Taken::Steers(_) | Taken::Sealed(_), _) => {
                panic!("round {round}: a steer was stranded between the check and the seal");
            }
        }
    }
}

#[test]
fn stop_and_seal_race_and_exactly_one_wins() {
    let route = route(model_config());
    for _ in 0..200 {
        let gate = SessionGate::new(1);
        let admission = held(&gate, &route);
        let cancellation = admission.cancellation();
        let barrier = Barrier::new(2);
        let (seal, stop) = std::thread::scope(|scope| {
            let sealing = scope.spawn(|| {
                barrier.wait();
                gate.seal(&key(), &cancellation)
            });
            barrier.wait();
            let stop = gate.cancel(&cancel(SUBJECT, CancelVia::StopReply));
            (sealing.join().expect("the seal never panics"), stop.running)
        });
        match (seal, stop) {
            (Seal::Sealed { .. }, CancelOutcome::Completing) => {
                assert!(!cancellation.is_cancelled());
            }
            (Seal::Stopped, CancelOutcome::Cancelled) => assert!(cancellation.is_cancelled()),
            (seal, stop) => panic!("both or neither won: {seal:?} and {stop:?}"),
        }
        assert_eq!(
            gate.seal(&key(), &cancellation),
            seal,
            "the outcome is final"
        );
        drop(admission);
    }
}

#[test]
fn consecutive_same_sender_followups_fold_and_another_sender_breaks_the_run() {
    let gate = SessionGate::new(1);
    let route = route(model_config());
    let admission = held(&gate, &route);
    for (subject, id, text) in [
        ("tel.999", "a1", "first part"),
        ("tel.999", "a2", "second part"),
        ("tel.888", "b1", "someone else"),
        ("tel.999", "a3", "after the break"),
    ] {
        assert!(matches!(
            admit(&gate, &route, native(subject, id, text)),
            Admit::Queued
        ));
    }

    let (admission, folded) = admission.next_or_release().expect("the folded run");
    assert_eq!(folded.message.subject.canonical(), "tel.999");
    assert_eq!(
        folded.message.message_id,
        MessageId::Native("a1".to_owned())
    );
    assert_eq!(folded.message.folded, ["a2"]);
    let first = folded.message.text.find("first part").expect("lead text");
    let second = folded
        .message
        .text
        .find("second part")
        .expect("folded text");
    assert!(first < second);
    let (admission, other) = admission.next_or_release().expect("the other sender");
    assert_eq!(other.message.text, "someone else");
    let (admission, last) = admission.next_or_release().expect("after the break");
    assert_eq!(last.message.text, "after the break");
    assert!(last.message.folded.is_empty());
    assert!(admission.next_or_release().is_none());
}

#[test]
fn the_mailbox_takes_a_hundred_steers_and_refuses_the_next_while_interrupts_stop_at_eight() {
    let gate = SessionGate::new(1);
    let mut route = route(model_config());
    route.steering = Steering::Abort;
    let admission = held(&gate, &route);
    let cancellation = admission.cancellation();
    let model = cancellation.model_watch();
    let mut interrupts = 0_u8;
    for _ in 0..MAILBOX_CAPACITY {
        assert!(matches!(
            admit(&gate, &route, message("steer")),
            Admit::Steered(_)
        ));
        if *model.borrow() {
            interrupts += 1;
        }
        cancellation.rearm_model();
    }
    assert_eq!(MAILBOX_CAPACITY, 100);
    assert_eq!(interrupts, MAX_INTERRUPTS);
    assert!(matches!(
        admit(&gate, &route, message("one too many")),
        Admit::Full(_, _)
    ));
    assert_eq!(gate.take_steers(&key()).len(), MAILBOX_CAPACITY);
}

struct HeldFirst {
    held: Arc<BlockedModel>,
    script: Arc<ModelScript>,
}

impl ModelFactory for Arc<HeldFirst> {
    fn build_bound(
        &self,
        _model: &Arc<ModelConfig>,
        _runtime: tokio::runtime::Handle,
        _cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        Ok(Arc::new(HeldFirstHandle(Arc::clone(self))))
    }
}

struct HeldFirstHandle(Arc<HeldFirst>);

impl ChatModel for HeldFirstHandle {
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError> {
        BlockedHandle(Arc::clone(&self.0.held)).complete(messages, tools, options, on_event)?;
        ScriptedModel(Arc::clone(&self.0.script)).complete(messages, tools, options, on_event)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_id_of_a_taken_folded_steer_is_consumed_and_never_replayed() {
    for steering in [Steering::Abort, Steering::Boundary] {
        let directory = temporary();
        let (broker, _asked) = naming_broker(directory.path(), &[(SUBJECT, "xavier")]).await;
        let script = ModelScript::new([answer("ok"), answer("ok"), answer("ok"), answer("ok")]);
        let models = Arc::new(HeldFirst {
            held: BlockedModel::new("held"),
            script: Arc::clone(&script),
        });
        let driver = HistoryDriver::new(Ok(vec![
            past("t2", "U1", SUBJECT, "lead"),
            past("t3", "U1", SUBJECT, "folded in"),
            past("t4", "U2", "tel.16035550102", "unaddressed"),
        ]));
        let runner = runner_with(broker, Arc::new(Arc::clone(&models)), 4);
        resident_since(&runner, "t1");
        let mut route = persistent_route(model_config(), recall_window(RecallSource::Platform));
        route.steering = steering;
        let run = |inbound| {
            run_session(
                Arc::clone(&runner),
                route.clone(),
                inbound,
                Arc::clone(&driver) as Arc<dyn ChatDriver>,
            )
        };

        let first = tokio::spawn(run(mention("t1", "first")));
        models.held.wait_until_entered().await;
        run(InboundMessage {
            folded: vec!["t3".to_owned()],
            ..mention("t2", "lead\n\nfolded in")
        })
        .await;
        models.held.release();
        first.await.expect("the first turn finishes");
        run(mention("t5", "next")).await;

        let prompt = script.prompt(script.requests() - 1);
        assert_eq!(
            prompt[prompt.len() - 2..],
            transcript(&[
                ("user", "[gateway: chat history, from U2]\nunaddressed"),
                ("user", "next"),
            ]),
            "{steering:?}"
        );
    }
}
