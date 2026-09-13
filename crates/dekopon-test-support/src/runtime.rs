//! A script runtime that parks where a real one would be waiting on a capability call.

use std::{
    sync::{
        Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    time::Duration,
};

use dekopon_agent::prompt::ScriptRuntime;
use dekopon_shell::{ExitCode, ScriptOutcome};
use tokio::sync::Notify;

/// Longest a parked script waits to be released before the fixture, not the subject, fails.
const PARK_CEILING: Duration = Duration::from_secs(30);

/// A [`ScriptRuntime`] that stops inside `run_script` until a test lets it finish.
///
/// This is the "during a parked tool" stage of the cancellation matrix, and the only way to reach
/// it deterministically. The prompt loop calls `run_script` on its blocking thread; the real
/// runtime is by then inside the broker leg, which has already emitted
/// `ProgressEvent::ToolStarted` for the command word it is invoking — the word is provider
/// vocabulary only that leg can construct, so a double cannot emit one and does not pretend to.
/// What it reproduces is the park that follows: a session that is demonstrably past tool start and
/// cannot reach its next cancellation boundary until the test says so.
///
/// The two halves are deliberately different primitives. Entry is announced with a
/// [`Notify`], because the waiting side is an async test that may be running on a
/// current-thread runtime with time paused, where a blocking receive would deadlock the runtime it
/// is parked on. The release travels back on a `std::sync::mpsc` channel, because the waiting side
/// there is the blocking prompt thread, which has no executor to yield to.
pub struct BlockedRuntime {
    entered: Notify,
    release: Mutex<Option<Sender<()>>>,
    release_signal: Mutex<Option<Receiver<()>>>,
    scripts: Mutex<Vec<String>>,
    outcome: ScriptOutcome,
    command_words: Vec<String>,
}

impl BlockedRuntime {
    /// A runtime that parks once, then reports `output` as a successful script.
    #[must_use]
    pub fn new(output: &str) -> Self {
        Self::with_outcome(ScriptOutcome {
            output: output.to_owned(),
            exit_code: ExitCode::SUCCESS,
            truncated: false,
            capability_calls: 1,
            steps: 1,
        })
    }

    /// A runtime that parks once, then reports exactly this outcome.
    fn with_outcome(outcome: ScriptOutcome) -> Self {
        let (release, release_signal) = channel();
        Self {
            entered: Notify::new(),
            release: Mutex::new(Some(release)),
            release_signal: Mutex::new(Some(release_signal)),
            scripts: Mutex::new(Vec::new()),
            outcome,
            command_words: Vec::new(),
        }
    }

    /// Offers these command words to the prompt loop, as a session holding grants would.
    #[must_use]
    pub fn offering(mut self, words: &[&str]) -> Self {
        self.command_words = words.iter().map(|word| (*word).to_owned()).collect();
        self
    }

    /// Resolves once a script has entered `run_script` and parked.
    pub async fn wait_until_parked(&self) {
        self.entered.notified().await;
    }

    /// Lets the parked script finish. Later calls do nothing, so a test may release twice.
    pub fn release(&self) {
        if let Some(sender) = self.release.lock().expect("release lock").take() {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "a runtime whose parked script already gave up is bounded by its own \
                          recv_timeout, which is where that failure is reported"
            )]
            let _ = sender.send(());
        }
    }

    /// Every script this runtime was asked to run, in order.
    #[must_use]
    pub fn scripts(&self) -> Vec<String> {
        self.scripts.lock().expect("script lock").clone()
    }
}

impl ScriptRuntime for BlockedRuntime {
    fn run_script(&self, script: &str, _max_capability_calls: u32) -> ScriptOutcome {
        self.scripts
            .lock()
            .expect("script lock")
            .push(script.to_owned());
        self.entered.notify_one();
        if let Some(receiver) = self.release_signal.lock().expect("release lock").take() {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "both outcomes end the park: a delivered release is the test proceeding, \
                          and the ceiling elapsing is the test having abandoned this session, \
                          which its own assertions then report"
            )]
            let _ = receiver.recv_timeout(PARK_CEILING);
        }
        self.outcome.clone()
    }

    fn command_words(&self) -> Vec<String> {
        self.command_words.clone()
    }
}
