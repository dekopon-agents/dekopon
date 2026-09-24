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

const PARK_CEILING: Duration = Duration::from_secs(30);

pub struct BlockedRuntime {
    entered: Notify,
    release: Mutex<Option<Sender<()>>>,
    release_signal: Mutex<Option<Receiver<()>>>,
    scripts: Mutex<Vec<String>>,
    outcome: ScriptOutcome,
    command_words: Vec<String>,
}

impl BlockedRuntime {
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

    #[must_use]
    pub fn offering(mut self, words: &[&str]) -> Self {
        self.command_words = words.iter().map(|word| (*word).to_owned()).collect();
        self
    }

    pub async fn wait_until_parked(&self) {
        self.entered.notified().await;
    }

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
