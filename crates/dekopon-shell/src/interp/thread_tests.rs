use super::{FatalError, nesting_tests::evaluator_in};
use crate::test_threads_up_to;
use crate::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal, RunCall, RunResult, Streams,
    parser::parse,
};
use dekopon_process::{CallBudget, LimitExceeded, Limits, ShellThreads, TreeContext};
use parking_lot::Mutex;
use std::{
    io::Read as _,
    sync::{
        Barrier,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct Probe {
    runs: AtomicU32,
    threads: Mutex<Vec<String>>,
    handoff: Option<Barrier>,
}

impl CapabilityInvoker for Probe {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }

    fn command_words(&self) -> Vec<String> {
        vec!["probe".to_owned()]
    }

    fn invoke(
        &self,
        _proposal: CommandProposal,
        _streams: Streams,
        _tree: &TreeContext,
    ) -> CapabilityCallResult {
        crate::secret_use_unsupported()
    }

    fn run(&self, call: RunCall, mut streams: Streams, _tree: &TreeContext) -> RunResult {
        self.runs.fetch_add(1, Ordering::Relaxed);
        let argv = call
            .argv
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        match argv.first().map(String::as_str) {
            Some("panic") => panic!("the provider panicked"),
            Some("sleep") => tokio::runtime::Handle::current()
                .block_on(tokio::time::sleep(Duration::from_millis(1))),
            Some("hold") => {
                let stdout = std::os::unix::net::UnixStream::from(streams.stdout);
                std::io::Write::write_all(&mut &stdout, b"x").unwrap();
                self.handoff.as_ref().unwrap().wait();
                streams.stdout = stdout.into();
            }
            Some("snapshot") => {
                let mut stdin = std::os::unix::net::UnixStream::from(streams.stdin.take().unwrap());
                stdin.read_exact(&mut [0; 1]).unwrap();
                *self.threads.lock() = thread_names();
                self.handoff.as_ref().unwrap().wait();
            }
            _ => {}
        }
        crate::render_command(streams, "", String::new(), 0)
    }
}

#[cfg(target_os = "linux")]
fn thread_names() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .map(|comm| comm.trim_end().to_owned())
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn thread_names() -> Vec<String> {
    Vec::new()
}

fn execute(probe: &Probe, threads: &ShellThreads, script: &str) -> Result<(), FatalError> {
    let limits = Limits::default();
    let tree = TreeContext::new(limits, CallBudget::new(8), threads.clone());
    let program = parse(script).unwrap();
    evaluator_in(probe, limits, tree)
        .execute_program(&program)
        .map(|_| ())
}

fn refused(maximum: u32) -> Result<(), FatalError> {
    Err(FatalError::Limit(LimitExceeded::ShellThreads { maximum }))
}

#[test]
fn a_pipeline_past_the_thread_cap_is_refused_whole_before_any_stage_runs() {
    let (probe, threads) = (Probe::default(), test_threads_up_to(1));
    assert_eq!(execute(&probe, &threads, "probe | probe | cat"), refused(1));
    assert_eq!(probe.runs.load(Ordering::Relaxed), 0);
    assert!(threads.reserve::<1>().is_ok());
}

#[test]
fn a_provider_call_past_the_thread_cap_is_refused_before_it_is_charged() {
    let probe = Probe::default();
    let threads = test_threads_up_to(1);
    let tree = TreeContext::new(Limits::default(), CallBudget::new(8), threads.clone());
    let program = parse("echo x | probe").unwrap();
    let outcome = evaluator_in(&probe, Limits::default(), tree.clone()).execute_program(&program);
    assert_eq!(outcome.map(|_| ()), refused(1));
    assert_eq!(
        (probe.runs.load(Ordering::Relaxed), tree.calls().used()),
        (0, 0)
    );
}

#[test]
fn a_jq_past_the_thread_cap_is_refused_with_the_typed_limit() {
    let threads = test_threads_up_to(2);
    assert_eq!(execute(&Probe::default(), &threads, "jq -n ."), refused(2));
}

#[test]
fn a_panicking_provider_returns_every_thread_permit() {
    let (probe, threads) = (Probe::default(), test_threads_up_to(4));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        execute(&probe, &threads, "probe panic | cat")
    }));
    assert!(panicked.is_err());
    assert!(threads.reserve::<4>().is_ok());
}

#[test]
fn a_provider_blocks_on_a_runtime_timer_from_a_pipeline_stage() {
    let threads = test_threads_up_to(4);
    assert_eq!(
        execute(&Probe::default(), &threads, "probe sleep | cat"),
        Ok(())
    );
}

#[cfg(target_os = "linux")]
#[test]
fn stage_call_and_feeder_threads_carry_their_full_names() {
    let probe = Probe {
        handoff: Some(Barrier::new(2)),
        ..Probe::default()
    };
    let threads = test_threads_up_to(4);
    assert_eq!(
        execute(&probe, &threads, "probe hold | probe snapshot"),
        Ok(())
    );
    let names = probe.threads.lock();
    for name in ["sh-stage", "sh-call", "sh-feed"] {
        assert!(names.iter().any(|seen| seen == name), "{name} in {names:?}");
    }
}

#[test]
fn a_feeder_that_never_started_fails_the_command() {
    let unstarted = super::Fed::Unstarted(std::io::Error::other("no thread"));
    assert!(matches!(
        unstarted.settle("probe"),
        Err(crate::builtins::CommandFailure::Status { status, .. }) if status != dekopon_process::ExitCode::SUCCESS
    ));
}
