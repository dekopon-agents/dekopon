use super::*;
use crate::jobs::{JobContext, JobOwner, Jobs};
use dekopon_broker_protocol::Trigger;
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, ExitCode, JobId, JobOutcome, JobState, JobSummary,
};

fn job_route(script_timeout: Duration, job_timeout: Duration) -> crate::routes::BoundRoute {
    crate::routes::BoundRoute {
        script_timeout,
        job_timeout: Some(job_timeout),
        ..route(model_config())
    }
}

fn runner_with_jobs(
    broker: ResolvedBroker,
    models: Arc<ModelScript>,
    max_concurrent: usize,
    max_jobs: usize,
) -> Arc<SessionRunner> {
    let mut runner = Arc::into_inner(runner(broker, models, max_concurrent))
        .expect("a fresh runner has one owner");
    runner.jobs = Arc::new(Jobs::new(max_jobs));
    Arc::new(runner)
}

async fn settled(jobs: &Jobs, owner: &JobOwner, within: Duration) -> Vec<JobSummary> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let rows = jobs.list(owner);
        let running = rows
            .iter()
            .any(|row| matches!(row.state, JobState::Running { .. }));
        if !running || tokio::time::Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn finished(row: &JobSummary) -> (JobOutcome, ExitCode) {
    match row.state {
        JobState::Finished { outcome, exit, .. } => (outcome, exit),
        JobState::Running { .. } => panic!("job {} is still running", row.id),
    }
}

fn job_triggers(observed: &mut mpsc::UnboundedReceiver<RequestEnvelope>) -> Vec<Trigger> {
    let mut triggers = Vec::new();
    while let Ok(request) = observed.try_recv() {
        if let BrokerRequest::Capabilities {
            attestation: Some(Attestation {
                scope: Some(scope), ..
            }),
        } = request.request
        {
            triggers.push(scope.trigger);
        }
    }
    triggers
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_outlives_its_turn_and_the_turns_script_deadline() {
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([script_call("sleep 2 && echo done &"), answer("started")]);
    let runner = runner(broker, Arc::clone(&models), 1);
    let driver = Arc::new(RecordingDriver::default());
    let started = Instant::now();
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_millis(1_000), Duration::from_secs(10)),
        message("start it"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    let turn_ended = started.elapsed();
    assert_eq!(driver.replies(), ["started"]);
    assert!(
        models
            .prompt(1)
            .iter()
            .any(|(role, text)| role == "tool" && text.contains("[1]")),
        "{:?}",
        models.prompt(1)
    );
    let owner = JobOwner::of(&message("start it"));
    let [row] = settled(&runner.jobs, &owner, Duration::from_secs(10))
        .await
        .try_into()
        .expect("one job");
    let ended = started.elapsed();
    assert_eq!(finished(&row), (JobOutcome::Succeeded, ExitCode::SUCCESS));
    assert_eq!(&*row.text, "sleep 2 && echo done");
    assert!(ended > turn_ended, "the job ended with its turn");
    assert!(ended > Duration::from_millis(1_000), "{ended:?}");
    assert_eq!(
        job_triggers(&mut observed),
        [Trigger::Message, Trigger::Job]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_start_past_max_jobs_fails_at_once_without_taking_a_session_permit() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(2, &["cli-probe.upper"])).await;
    let models = ModelScript::new([
        script_call("sleep 5 &\nsleep 5 &\necho status=$?"),
        answer("done"),
    ]);
    let runner = runner_with_jobs(broker, Arc::clone(&models), 1, 1);
    let driver = Arc::new(RecordingDriver::default());
    let started = Instant::now();
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(30), Duration::from_secs(30)),
        message("two"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let tool = models
        .prompt(1)
        .into_iter()
        .find(|(role, _)| role == "tool")
        .map(|(_, text)| text)
        .expect("the script's output");
    assert!(
        tool.contains(
            "1 jobs are already running, the most this gateway allows (sessions.maxJobs)"
        ),
        "{tool}"
    );
    assert!(tool.contains("status=1"), "{tool}");
    assert!(
        runner
            .gate
            .admit_probe(("dev".to_owned(), "elsewhere".to_owned()))
            .is_some(),
        "a running job holds a session permit"
    );
    let owner = JobOwner::of(&message("two"));
    runner.jobs.kill(&owner, JobId::new(1)).expect("kill");
    let [row] = settled(&runner.jobs, &owner, Duration::from_secs(5))
        .await
        .try_into()
        .expect("one job");
    assert_eq!(finished(&row).0, JobOutcome::Killed);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_self_restarting_job_chain_ends_at_the_root_jobs_deadline() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(12, &["cli-probe.upper"])).await;
    let models = ModelScript::new([script_call("f() { sleep 1; f & }; f &"), answer("looping")]);
    let runner = runner(broker, Arc::clone(&models), 1);
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        Arc::clone(&runner),
        job_route(Duration::from_secs(30), Duration::from_millis(3_000)),
        message("loop"),
        Arc::clone(&driver) as Arc<dyn ChatDriver>,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let rows = runner.jobs.list(&JobOwner::of(&message("loop")));
    assert!(!rows.is_empty());
    for row in &rows {
        let (outcome, _) = finished(row);
        assert!(
            matches!(outcome, JobOutcome::Deadline | JobOutcome::Succeeded),
            "{row:?}"
        );
    }
    assert!(
        rows.iter()
            .any(|row| finished(row).0 == JobOutcome::Deadline),
        "{rows:?}"
    );
}

struct ProbeInvoker(JobContext);

impl CapabilityInvoker for ProbeInvoker {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }

    fn job_control(&self) -> Option<&dyn dekopon_shell::JobControl> {
        Some(&self.0)
    }

    fn invoke(&self, _: dekopon_shell::CommandProposal) -> CapabilityCallResult {
        CapabilityCallResult::NotFound
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_a_probe_starts_attests_as_a_probe() {
    let directory = temporary();
    let (broker, mut observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let jobs = Arc::new(Jobs::new(2));
    let inbound = message("watch");
    let route = route(model_config());
    let anchor = crate::wake::Anchor::for_job(&inbound, &route.agent).expect("an anchor");
    let context = JobContext::for_turn(
        Arc::clone(&jobs),
        &inbound,
        anchor,
        broker,
        dekopon_shell::Limits::default(),
    )
    .for_probe();
    let outcome = tokio::task::spawn_blocking(move || {
        dekopon_shell::Interpreter::new(dekopon_shell::Limits::default())
            .run("true &", &ProbeInvoker(context))
    })
    .await
    .expect("the probe script joins");
    assert_eq!(outcome.output, "[1]");
    let [row] = settled(&jobs, &JobOwner::of(&inbound), Duration::from_secs(5))
        .await
        .try_into()
        .expect("one job");
    assert_eq!(finished(&row).0, JobOutcome::Succeeded);
    assert_eq!(job_triggers(&mut observed), [Trigger::Probe]);
}

fn subject_named(name: &str) -> ExternalSubject {
    let number = if name == "a" {
        "16030000001"
    } else {
        "16030000002"
    };
    ExternalSubject::telephone(number).expect("a telephone subject")
}

fn admitted(
    jobs: &Arc<Jobs>,
    subject_text: &str,
    text: &str,
) -> Result<crate::jobs::JobRun, dekopon_shell::JobRefusal> {
    let mut inbound = message(subject_text);
    inbound.subject = subject_named(subject_text);
    let anchor =
        crate::wake::Anchor::for_job(&inbound, &route(model_config()).agent).expect("an anchor");
    jobs.admit(JobOwner::of(&inbound), anchor, None, Arc::from(text))
        .map(|(run, _signal)| run)
}

fn owned_by(subject_text: &str) -> JobOwner {
    let mut inbound = message(subject_text);
    inbound.subject = subject_named(subject_text);
    JobOwner::of(&inbound)
}

#[test]
fn dropping_an_unfinished_run_fails_its_row_and_returns_the_permit() {
    let jobs = Arc::new(Jobs::new(1));
    let run = admitted(&jobs, "a", "sleep 9").expect("a free slot");
    assert_eq!(jobs.free_permits(), 0);
    drop(run);
    assert_eq!(jobs.free_permits(), 1);
    let [row] = jobs.list(&owned_by("a")).try_into().expect("one row");
    assert_eq!(finished(&row), (JobOutcome::Failed, ExitCode::FAILURE));
}

#[test]
fn a_full_table_evicts_its_oldest_finished_row_and_never_a_running_one() {
    let jobs = Arc::new(Jobs::new(2));
    let first = admitted(&jobs, "a", "a").expect("slot");
    let second = admitted(&jobs, "b", "b").expect("slot");
    assert!(admitted(&jobs, "a", "c").is_err());
    drop(first);
    let third = admitted(&jobs, "a", "c").expect("slot");
    let texts = |subject| {
        jobs.list(&owned_by(subject))
            .into_iter()
            .map(|row| row.text.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(texts("a"), ["c"]);
    assert_eq!(texts("b"), ["b"]);
    drop((second, third));
}
