use super::*;
use parking_lot::Mutex;
use std::collections::BTreeMap;

#[derive(Default)]
struct Commands(Mutex<Vec<Vec<Arg>>>);
impl CapabilityInvoker for Commands {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }
    fn command_words(&self) -> Vec<String> {
        vec!["asset".to_owned()]
    }
    fn command_word_help(&self) -> BTreeMap<String, String> {
        BTreeMap::from([("asset".to_owned(), "cached help\n".to_owned())])
    }
    fn run(&self, call: RunCall, streams: Streams, _: &TreeContext) -> RunResult {
        self.0.lock().push(call.argv);
        render_command(streams, "", String::new(), 2)
    }
    fn invoke(&self, _: CommandProposal, _: Streams, _: &TreeContext) -> CapabilityCallResult {
        panic!("Run owns dispatch")
    }
}

#[test]
fn ifs_never_splits_a_reference() {
    let commands = Commands::default();
    let outcome = Interpreter::new(Limits::default()).run(
        "IFS='chat-asset:3'; asset chat-asset:3 'chat-asset:3' --image=chat-asset:3",
        &commands,
    );
    assert_eq!(outcome.exit_code.get(), 2);
    assert_eq!(
        *commands.0.lock(),
        [vec![
            Arg::Asset(dekopon_core::AssetNumber::parse("chat-asset:3").unwrap()),
            Arg::Text("chat-asset:3".to_owned()),
            Arg::Text("--image=chat-asset:3".to_owned())
        ]]
    );
}

#[test]
fn a_reference_outside_argv_expands_to_its_text() {
    let commands = Commands::default();
    let outcome = Interpreter::new(Limits::default()).run("x=chat-asset:3; echo $x; for y in chat-asset:3; do echo $y; done; case chat-asset:3 in chat-asset:3) echo yes;; esac; printf '%s' chat-asset:3", &commands);
    assert_eq!(outcome.exit_code.get(), 0);
    assert_eq!(
        outcome.output,
        "chat-asset:3\nchat-asset:3\nyes\nchat-asset:3"
    );
    assert!(commands.0.lock().is_empty());
}

#[test]
fn a_builtin_names_asset_cat_for_a_reference_operand() {
    for command in ["head", "cat"] {
        let outcome = Interpreter::new(Limits::default()).run(
            &format!("echo buffered > chat-asset:3; {command} chat-asset:3"),
            &Commands::default(),
        );
        assert_eq!(
            outcome.output,
            format!(
                "{command}: chat-asset:3 is an attachment, not a buffer; read it with `asset cat chat-asset:3 | {command}`"
            )
        );
    }
}

#[test]
fn a_literal_help_sends_no_frame() {
    let commands = Commands::default();
    let outcome = Interpreter::new(Limits {
        max_capability_calls: 0,
        ..Limits::default()
    })
    .run("asset --help", &commands);
    assert_eq!(outcome.exit_code.get(), 0);
    assert_eq!(outcome.output, "cached help");
    assert!(commands.0.lock().is_empty());
}

#[test]
fn a_usage_error_refunds_its_call() {
    let commands = Commands::default();
    let calls = CallBudget::new(1);
    let tree = TreeContext::new(Limits::default(), calls.clone());
    let outcome = Interpreter::new(Limits::default()).run_with_tree(
        "asset invalid; asset invalid",
        &commands,
        &tree,
    );
    assert_eq!(outcome.exit_code.get(), 2);
    assert_eq!(commands.0.lock().len(), 2);
    assert_eq!(calls.used(), 0);
}

#[test]
fn concurrent_stages_cannot_overrun_the_call_budget() {
    #[derive(Default)]
    struct Probe {
        runs: Arc<Mutex<usize>>,
        job: Mutex<Option<std::thread::JoinHandle<ExitCode>>>,
        started: std::sync::atomic::AtomicBool,
    }
    impl JobControl for Probe {
        fn start(&self, seed: JobSeed) -> Result<JobId, JobRefusal> {
            assert!(!self.started.swap(true, std::sync::atomic::Ordering::SeqCst));
            let probe = Probe {
                runs: Arc::clone(&self.runs),
                job: Mutex::new(None),
                started: std::sync::atomic::AtomicBool::new(false),
            };
            let tree = seed.tree().clone();
            *self.job.lock() = Some(std::thread::spawn(move || {
                Interpreter::new(Limits::default())
                    .run_seed(seed, &probe, &tree)
                    .exit_code
            }));
            Ok(JobId::new(1))
        }
        fn list(&self) -> Vec<JobSummary> {
            Vec::new()
        }
        fn wait(&self, ids: &[JobId], _: &dyn Fn() -> bool) -> Vec<Result<JobWait, JobRefusal>> {
            assert_eq!(ids, [JobId::new(1)]);
            let job = self.job.lock().take().unwrap();
            vec![Ok(JobWait::Exited(job.join().unwrap()))]
        }
        fn kill(&self, _: JobId) -> Result<(), JobRefusal> {
            panic!("this script waits for its job")
        }
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            if let Some(job) = self.job.get_mut().take() {
                job.join().unwrap();
            }
        }
    }
    impl CapabilityInvoker for Probe {
        fn job_control(&self) -> Option<&dyn JobControl> {
            Some(self)
        }
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }
        fn command_words(&self) -> Vec<String> {
            vec!["probe".to_owned()]
        }
        fn run(&self, call: RunCall, streams: Streams, _: &TreeContext) -> RunResult {
            assert_eq!(call.word, "probe");
            *self.runs.lock() += 1;
            drop(streams);
            RunResult::Invoked {
                capability: "probe".to_owned(),
                input: Value::Null,
                secret_use: None,
                result: CapabilityCallResult::Succeeded,
            }
        }
        fn invoke(&self, _: CommandProposal, _: Streams, _: &TreeContext) -> CapabilityCallResult {
            panic!("Run owns dispatch")
        }
    }
    let probe = Probe::default();
    let calls = CallBudget::new(1);
    let tree = TreeContext::new(Limits::default(), calls.clone());
    Interpreter::new(Limits::default()).run_with_tree("probe & probe; wait", &probe, &tree);
    let unfinished = probe.job.lock().take();
    if let Some(job) = unfinished {
        job.join().unwrap();
    }
    assert!(probe.started.load(std::sync::atomic::Ordering::SeqCst));
    assert!(probe.job.lock().is_none());
    assert_eq!(*probe.runs.lock(), 1);
    assert_eq!(calls.used(), 1);
}

#[test]
fn a_render_failure_records_its_io_cause_once() {
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let captured = Capture(Arc::default());
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    let (output, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(peer);
    let result = tracing::subscriber::with_default(subscriber, || {
        render_command(
            Streams {
                stdin: None,
                stdout: output.into(),
            },
            "output",
            String::new(),
            0,
        )
    });
    match result {
        RunResult::Returned(CapabilityCallResult::Failed { error, detail }) => {
            assert_eq!(error, "command stdout could not be written");
            assert!(detail.is_none());
        }
        RunResult::Invoked { .. }
        | RunResult::Uncertain(_)
        | RunResult::Returned(
            CapabilityCallResult::Succeeded
            | CapabilityCallResult::SucceededWithStderr(_)
            | CapabilityCallResult::Exited { .. }
            | CapabilityCallResult::Denied { .. }
            | CapabilityCallResult::NotFound,
        ) => panic!("a closed stdout must return a failure"),
    }
    let bytes = captured.0.lock();
    let events = serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["fields"]["event"], "shell_command_stdout_failed");
    assert_eq!(events[0]["fields"]["error.kind"], "BrokenPipe");
    assert_eq!(events[0]["fields"]["errno"], 32);
}
