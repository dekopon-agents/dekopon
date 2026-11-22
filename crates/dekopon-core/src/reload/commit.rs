use tokio::sync::watch;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct GenerationDigest(String);

impl GenerationDigest {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn from_hex(digest: String) -> Option<Self> {
        todo!()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[expect(dead_code, reason = "skeleton: daemon/broker-self-reload")]
pub struct CommitSignal(watch::Sender<GenerationDigest>);

#[derive(Clone)]
#[expect(dead_code, reason = "skeleton: daemon/broker-self-reload")]
pub struct CommitReceiver(watch::Receiver<GenerationDigest>);

impl CommitSignal {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/broker-self-reload"
    )]
    pub fn new(active: GenerationDigest) -> Self {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/broker-self-reload"
    )]
    pub fn publish(&self, committed: GenerationDigest) {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/broker-self-reload")]
    pub fn subscribe(&self) -> CommitReceiver {
        todo!()
    }
}

impl CommitReceiver {
    #[expect(clippy::todo, reason = "skeleton: daemon/broker-self-reload")]
    pub async fn changed(&mut self) -> Option<GenerationDigest> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/broker-self-reload")]
    pub fn current(&self) -> GenerationDigest {
        todo!()
    }
}
