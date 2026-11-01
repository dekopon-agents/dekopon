use super::jobs_e2e::{
    attested, finished, from, job_route, job_routes, runner_with_jobs, settled, tool_output_of,
    until,
};
use super::*;
use crate::jobs::JobOwner;
use dekopon_broker_protocol::{Arg, Trigger};
use dekopon_process::{ExitCode, JobId, JobOutcome, JobState};
use dekopon_test_support::{CaptureLayer, Record};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::prelude::*;

fn capture_job_records() -> (CaptureLayer, tracing::Dispatch) {
    let capture = CaptureLayer::workspace();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(capture.clone()));
    (capture, dispatch)
}

async fn notices_until(
    notices: &mut mpsc::Receiver<InboundMessage>,
    count: usize,
    within: Duration,
) -> Vec<InboundMessage> {
    tokio::time::timeout(within, async {
        let mut received = Vec::new();
        for _ in 0..count {
            received.push(notices.recv().await.expect("job notice"));
        }
        received
    })
    .await
    .expect("notices within observation window")
}

fn event<'a>(records: &'a [Record], name: &str) -> &'a Record {
    let rows: Vec<_> = records
        .iter()
        .filter(|r| r.field("message") == Some(name))
        .collect();
    assert_eq!(rows.len(), 1, "{records:?}");
    rows[0]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_third_start_at_the_default_limit_is_refused_across_two_people_and_succeeds_once_one_ends()
 {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(6, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 30 &\nsleep 30 &"),
        answer("A"),
        script_call("sleep 1 &\necho status=$?"),
        answer("B"),
        script_call("sleep 1 &\nwait\necho status=$?"),
        answer("B again"),
    ]);
    let runner = runner_with_jobs(
        broker,
        Arc::clone(&models),
        2,
        crate::config::DEFAULT_MAX_JOBS,
    );
    let route = job_route(Duration::from_secs(10), Duration::from_secs(30));
    let a = from("a", "start");
    let mut b = from("b", "start");
    b.conversation.id = "other".to_owned();
    let (oa, ob) = (JobOwner::of(&a), JobOwner::of(&b));
    tool_output_of(&runner, &models, route.clone(), a).await;
    let start = Instant::now();
    let output = tool_output_of(&runner, &models, route.clone(), b.clone()).await;
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(output.lines().any(|line| line == "status=1"), "{output}");
    assert!(runner.jobs.list(&ob).is_empty());
    let rows = runner.jobs.list(&oa);
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|r| matches!(r.state, JobState::Running { .. }))
    );
    assert_eq!(runner.jobs.free_permits(), 0);
    runner.jobs.kill(&oa, JobId::new(1)).unwrap();
    until(Duration::from_secs(3), || {
        matches!(runner.jobs.list(&oa)[0].state, JobState::Finished { .. })
    })
    .await;
    assert_eq!(finished(&runner.jobs.list(&oa)[0]).0, JobOutcome::Killed);
    let output = tool_output_of(&runner, &models, route, b).await;
    assert!(output.lines().any(|line| line == "status=0"), "{output}");
    assert_eq!(
        finished(&runner.jobs.list(&ob)[0]),
        (JobOutcome::Succeeded, ExitCode::SUCCESS)
    );
    let rows = runner.jobs.list(&oa);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, JobId::new(2));
    assert!(matches!(rows[0].state, JobState::Running { .. }));
    runner.jobs.kill(&oa, JobId::new(2)).unwrap();
    settled(&runner.jobs, &oa, Duration::from_secs(3)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_past_its_deadline_ends_deadline_with_exit_124_and_sends_one_notice() {
    let (capture, dispatch) = capture_job_records();
    async {
        let directory = temporary();
        let (broker, _) = stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
        let models = ModelScript::new([script_call("sleep 10 &"), answer("started")]);
        let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
        let mut notices = runner.jobs.take_notices();
        let inbound = message("start");
        let owner = JobOwner::of(&inbound);
        tool_output_of(&runner, &models, job_route(Duration::from_secs(5), Duration::from_secs(2)), inbound).await;
        let notices_received = notices_until(&mut notices, 1, Duration::from_secs(4)).await;
        assert!(matches!(notices_received[0].message_id, MessageId::Job { id, .. } if id == JobId::new(1)));
        assert_eq!(finished(&runner.jobs.list(&owner)[0]), (JobOutcome::Deadline, ExitCode::TIMEOUT));
        assert_eq!(runner.jobs.free_permits(), 2);
        assert!(notices.try_recv().is_err());
    }.with_subscriber(dispatch).await;
    let records = capture.records();
    let row = event(&records, "job.finished");
    assert_eq!(row.field("job.outcome"), Some("deadline"));
    assert_eq!(row.field("job.exit_code"), Some("124"));
    assert!((2000..3000).contains(&row.field("duration_ms").unwrap().parse::<u64>().unwrap()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_that_exits_nonzero_ends_failed_with_its_code_in_record_and_notice() {
    let (capture, dispatch) = capture_job_records();
    async {
        let directory = temporary();
        let (broker, _) = stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
        let models = ModelScript::new([
            script_call("f() { sleep 1; return 3; }\nf &"),
            answer("started"),
        ]);
        let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
        let mut notices = runner.jobs.take_notices();
        let inbound = message("start");
        let owner = JobOwner::of(&inbound);
        tool_output_of(
            &runner,
            &models,
            job_route(Duration::from_secs(5), Duration::from_secs(10)),
            inbound,
        )
        .await;
        let received = notices_until(&mut notices, 1, Duration::from_secs(3)).await;
        assert!(matches!(received[0].message_id, MessageId::Job { id, .. } if id == JobId::new(1)));
        assert!(
            received[0]
                .text
                .starts_with("[gateway: job 1 finished, exit 3, after ")
        );
        assert_eq!(
            finished(&runner.jobs.list(&owner)[0]),
            (JobOutcome::Failed, ExitCode::from_script_exit(3))
        );
        assert!(notices.try_recv().is_err());
    }
    .with_subscriber(dispatch)
    .await;
    let records = capture.records();
    let row = event(&records, "job.finished");
    assert_eq!(row.field("job.outcome"), Some("failed"));
    assert_eq!(row.field("job.exit_code"), Some("3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn each_jobs_output_reaches_only_its_own_notice_in_its_own_conversation() {
    let (capture, dispatch) = capture_job_records();
    async {
        let directory = temporary();
        let (broker, _) = stub_broker(directory.path(), listings(6, &["cli-probe.upper"])).await;
        let models = ModelScript::new([script_call("a() { sleep 1; echo alpha-only; }\na &"), answer("A"), script_call("b() { echo bravo-only; return 5; }\nb &"), answer("B"), answer("noticed B"), answer("noticed A")]);
        let runner = runner_with_jobs(broker, Arc::clone(&models), 2, 2);
        let mut notices = runner.jobs.take_notices();
        let a = from("a", "start");
        let mut b = from("b", "start");
        b.conversation.id = "other".to_owned();
        let route = job_route(Duration::from_secs(5), Duration::from_secs(10));
        tool_output_of(&runner, &models, route.clone(), a.clone()).await;
        tool_output_of(&runner, &models, route.clone(), b.clone()).await;
        let received = notices_until(&mut notices, 2, Duration::from_secs(3)).await;
        for (id, inbound, output, exit) in [(1, a, "alpha-only", ExitCode::SUCCESS), (2, b, "bravo-only", ExitCode::from_script_exit(5))] {
            let notice = received.iter().find(|n| matches!(n.message_id, MessageId::Job { id: job, .. } if job == JobId::new(id))).unwrap();
            assert_eq!(notice.conversation.key(), inbound.conversation.key());
            assert_eq!(notice.subject, inbound.subject);
            assert_eq!(notice.text.lines().last(), Some(output));
            assert!(!notice.text.contains(if id == 1 { "bravo-only" } else { "alpha-only" }));
            assert_eq!(finished(&runner.jobs.list(&JobOwner::of(&inbound))[0]).1, exit);
            run_session(Arc::clone(&runner), route.clone(), notice.clone(), Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>).await;
        }
        assert!(notices.try_recv().is_err());
    }.with_subscriber(dispatch).await;
    let records = capture.records();
    let notices: Vec<_> = records
        .iter()
        .filter(|r| r.field("message") == Some("job.notice"))
        .collect();
    assert_eq!(notices.len(), 2);
    for id in ["1", "2"] {
        assert_eq!(
            notices
                .iter()
                .filter(|r| r.field("job.id") == Some(id))
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_jobs_write_is_proposed_on_a_leg_attested_as_job_and_its_notice_turn_too() {
    let directory = temporary();
    let proposal = ResponseEnvelope::command_run(
        serde_json::from_value(
            json!({"outcome":"proposed", "capability":"cli-probe.write", "input":{"text":"x"}}),
        )
        .unwrap(),
    );
    let listing = || {
        ResponseEnvelope::capabilities(
            vec![capability("cli-probe.write")],
            vec!["probe".to_owned()],
            BTreeMap::new(),
        )
    };
    let mut responses = vec![listing(), listing()];
    responses.extend([
        proposal,
        ResponseEnvelope::invocation(record_success(), Vec::new(), Vec::new(), Vec::new()),
    ]);
    responses.push(listing());
    let (broker, mut observed) = stub_broker(directory.path(), responses).await;
    let models = ModelScript::new([
        script_call("probe write x &"),
        answer("started"),
        answer("noted"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let mut notices = runner.jobs.take_notices();
    let inbound = message("start");
    let owner = JobOwner::of(&inbound);
    let route = job_route(Duration::from_secs(5), Duration::from_secs(10));
    tool_output_of(&runner, &models, route.clone(), inbound).await;
    let notice = notices_until(&mut notices, 1, Duration::from_secs(3))
        .await
        .pop()
        .unwrap();
    run_session(
        Arc::clone(&runner),
        route,
        notice,
        Arc::new(RecordingDriver::default()) as Arc<dyn ChatDriver>,
    )
    .await;
    assert_eq!(
        finished(&runner.jobs.list(&owner)[0]).0,
        JobOutcome::Succeeded
    );
    let requests: Vec<_> = std::iter::from_fn(|| observed.try_recv().ok()).collect();
    let invokes: Vec<_> = requests
        .iter()
        .filter_map(|r| {
            if let BrokerRequest::Run {
                word,
                argv,
                attestation: Some(attestation),
                ..
            } = &r.request
            {
                Some((word, argv, attestation))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(invokes.len(), 1);
    assert_eq!(invokes[0].0, "probe");
    assert_eq!(
        invokes[0].1,
        &vec![Arg::Text("write".to_owned()), Arg::Text("x".to_owned())]
    );
    assert_eq!(invokes[0].2.scope.as_ref().unwrap().trigger, Trigger::Job);
    let (send, mut recv) = mpsc::unbounded_channel();
    for request in requests {
        send.send(request).unwrap();
    }
    assert_eq!(
        attested(&mut recv),
        (
            vec![Trigger::Message, Trigger::Job, Trigger::Job],
            vec![Trigger::Job]
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_started_by_a_job_spends_its_starters_call_budget_and_a_new_turn_job_gets_a_fresh_one()
 {
    let directory = temporary();
    let mut responses = vec![probe_listing(), probe_listing()];
    for text in ["a", "b"] {
        responses.extend([
            upper_proposal(text),
            ResponseEnvelope::invocation(record_success(), Vec::new(), Vec::new(), Vec::new()),
        ]);
    }
    responses.extend([probe_listing()]);
    responses.extend([
        upper_proposal("c"),
        ResponseEnvelope::invocation(record_success(), Vec::new(), Vec::new(), Vec::new()),
    ]);
    responses.extend(vec![probe_listing(), probe_listing()]);
    responses.extend([
        upper_proposal("e"),
        ResponseEnvelope::invocation(record_success(), Vec::new(), Vec::new(), Vec::new()),
    ]);
    let (broker, mut observed) = stub_broker(directory.path(), responses).await;
    let models = ModelScript::new([
        script_call(
            "h() { probe upper --text c; probe upper --text d; }\ng() { probe upper --text a; probe upper --text b; h &\nwait; }\ng &",
        ),
        answer("started"),
        script_call("probe upper --text e &"),
        answer("again"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 3);
    let mut notices = runner.jobs.take_notices();
    let inbound = message("start");
    let owner = JobOwner::of(&inbound);
    let mut route = job_route(Duration::from_secs(5), Duration::from_secs(10));
    route.limits.max_capability_calls = 3;
    tool_output_of(&runner, &models, route.clone(), inbound.clone()).await;
    notices_until(&mut notices, 1, Duration::from_secs(3)).await;
    let rows = settled(&runner.jobs, &owner, Duration::from_secs(3)).await;
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(finished(&row), (JobOutcome::Failed, ExitCode::SYNTAX));
    }
    let first: Vec<_> = std::iter::from_fn(|| observed.try_recv().ok())
        .filter_map(|r| {
            if let BrokerRequest::Run { argv, .. } = r.request {
                Some(argv)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        first,
        ["a", "b", "c"].map(|text| vec![
            Arg::Text("upper".to_owned()),
            Arg::Text("--text".to_owned()),
            Arg::Text(text.to_owned())
        ])
    );
    tool_output_of(&runner, &models, route, inbound).await;
    notices_until(&mut notices, 1, Duration::from_secs(3)).await;
    let rows = settled(&runner.jobs, &owner, Duration::from_secs(3)).await;
    let row = rows.iter().find(|r| r.id == JobId::new(3)).unwrap();
    assert_eq!(finished(row), (JobOutcome::Succeeded, ExitCode::SUCCESS));
    let second: Vec<_> = std::iter::from_fn(|| observed.try_recv().ok())
        .filter_map(|r| {
            if let BrokerRequest::Run { argv, .. } = r.request {
                Some(argv)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        second,
        [vec![
            Arg::Text("upper".to_owned()),
            Arg::Text("--text".to_owned()),
            Arg::Text("e".to_owned())
        ]]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_person_in_two_conversations_sees_and_stops_only_that_conversations_jobs() {
    let directory = temporary();
    let routes = job_routes(directory.path(), Some(30_000)).await;
    let (broker, _) = stub_broker(directory.path(), listings(3, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 30 &"),
        answer("started"),
        script_call("jobs -p\necho rows=$?\nwait %1\necho waited=$?"),
        answer("listed"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
    let mut notices = runner.jobs.take_notices();
    let x = message("start");
    let owner = JobOwner::of(&x);
    let mut y = x.clone();
    y.conversation.id = "other".to_owned();
    let route = job_route(Duration::from_secs(5), Duration::from_secs(30));
    tool_output_of(&runner, &models, route.clone(), x.clone()).await;
    let anchor = crate::wake::Anchor::for_job(&x, &route.agent).unwrap();
    let (held, signal) = runner
        .jobs
        .admit(
            owner.clone(),
            anchor,
            None,
            Arc::from("sleep 30"),
            &runner.shell_threads,
        )
        .map(|admitted| (admitted.run, admitted.signal))
        .unwrap();
    let output = tool_output_of(&runner, &models, route, y.clone()).await;
    assert_eq!(output.lines().next(), Some("rows=0"));
    assert!(output.lines().any(|line| line == "waited=1"), "{output}");
    let mut sessions = tokio::task::JoinSet::new();
    let y_id = y.conversation.id.clone();
    for mut inbound in [y, x] {
        inbound.text = "stop".to_owned();
        crate::dispatch(
            &runner,
            &routes,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &["stop".to_owned()],
            &mut sessions,
            &mut crate::collection::Collector::new(&[], 4),
            &mut crate::follow::Debounce::default(),
            inbound.clone(),
        );
        if inbound.conversation.id == y_id {
            assert!(!signal.is_cancelled());
        }
    }
    assert!(signal.is_cancelled());
    drop(held);
    let rows = settled(&runner.jobs, &owner, Duration::from_secs(3)).await;
    assert_eq!(finished(&rows[0]).0, JobOutcome::Cancelled);
    assert!(notices.try_recv().is_err());
    while let Some(result) = sessions.join_next().await {
        result.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_hour_job_timeout_is_the_deadline_a_turns_job_records() {
    let (capture, dispatch) = capture_job_records();
    async {
        let directory = temporary();
        let (broker, _) = stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
        let models = ModelScript::new([script_call("sleep 60 &"), answer("started")]);
        let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
        let mut notices = runner.jobs.take_notices();
        let inbound = message("start");
        let owner = JobOwner::of(&inbound);
        tool_output_of(
            &runner,
            &models,
            job_route(Duration::from_secs(10), Duration::from_secs(3600)),
            inbound,
        )
        .await;
        runner.jobs.kill(&owner, JobId::new(1)).unwrap();
        let rows = settled(&runner.jobs, &owner, Duration::from_secs(3)).await;
        assert_eq!(finished(&rows[0]).0, JobOutcome::Killed);
        assert!(notices.try_recv().is_err());
    }
    .with_subscriber(dispatch)
    .await;
    let records = capture.records();
    let row = event(&records, "job.started");
    assert!(
        (3_595_000..=3_600_000).contains(
            &row.field("job.deadline_ms")
                .unwrap()
                .parse::<u64>()
                .unwrap()
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn job_words_on_a_route_without_jobs_report_off_while_foreign_job_words_are_refused() {
    let (capture, dispatch) = capture_job_records();
    async {
        let directory = temporary();
        let mut replies = listings(4, &["cli-probe.upper"]);
        replies.push(ResponseEnvelope {
            api_version: dekopon_broker_protocol::ProtocolVersion::V1Alpha2,
            response: dekopon_broker_protocol::BrokerResponse::Ran {
                outcome: dekopon_broker_protocol::RunOutcome::NotFound,
            },
        });
        let (broker, _) = stub_broker(directory.path(), replies).await;
        let models = ModelScript::new([
            script_call("sleep 1 &\njobs\nwait\nkill %1"),
            answer("off"),
            script_call("fg %1"),
            answer("fg"),
            script_call("bg"),
            answer("bg"),
            script_call("disown"),
            answer("disown"),
        ]);
        let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 2);
        for _ in 0..4 {
            tool_output_of(&runner, &models, route(model_config()), message("try")).await;
        }
        assert!(runner.jobs.list(&JobOwner::of(&message("try"))).is_empty());
    }
    .with_subscriber(dispatch)
    .await;
    let records = capture.records();
    assert!(
        !records
            .iter()
            .any(|r| r.field("message") == Some("job.started"))
    );
    for (name, kind, exit) in [
        ("jobs", "builtin", ExitCode::FAILURE),
        ("wait", "builtin", ExitCode::FAILURE),
        ("kill", "builtin", ExitCode::FAILURE),
        ("fg", "rejected", ExitCode::SYNTAX),
        ("bg", "rejected", ExitCode::SYNTAX),
        ("disown", "not-found", ExitCode::NOT_FOUND),
    ] {
        let rows: Vec<_> = records
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    Record::Span {
                        name: "shell.command",
                        ..
                    }
                ) && r.field("shell.command.name") == Some(name)
            })
            .collect();
        assert_eq!(rows.len(), 1, "{records:?}");
        assert_eq!(rows[0].field("shell.command.kind"), Some(kind));
        let index = records
            .iter()
            .position(|r| std::ptr::eq(r, rows[0]))
            .unwrap();
        let updates: Vec<_> = records[index + 1..]
            .iter()
            .take_while(|r| r.field("shell.command.name").is_none())
            .filter_map(|r| r.field("shell.command.exit_code"))
            .map(|code| ExitCode::from_script_exit(code.parse().unwrap()))
            .collect();
        assert_eq!(updates, [exit]);
    }
}
