#![expect(dead_code, reason = "skeleton: daemon/watcher-core")]

use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, atomic::AtomicBool},
};

use tokio::sync::{Notify, mpsc};

use super::{Cause, Reconcile, ReloadError, ReloadSettings, WatchSet, path::Registration};

const EVENT_QUEUE: usize = 64;

enum Event {
    Changed(PathBuf),
    Rescan,
    Hangup,
}

// notify's thread never blocks on the queue: a full queue sets `overflowed` and a backend error sets
// `failed`, and both wake the loop through `wake`, so neither can be lost behind a full queue.
struct Shared {
    overflowed: AtomicBool,
    failed: OnceLock<io::ErrorKind>,
    wake: Notify,
}

enum Backend {
    Notify(notify::RecommendedWatcher),
    Injected,
}

enum Hangup {
    Signal(tokio::signal::unix::Signal),
    Injected,
}

struct Slot<G> {
    group: G,
    watching: WatchSet,
    pending: Option<Cause>,
}

pub struct Watcher<G: Reconcile> {
    settings: ReloadSettings,
    slots: Vec<Slot<G>>,
    registered: BTreeSet<Registration>,
    backend: Backend,
    events: mpsc::Receiver<Event>,
    shared: Arc<Shared>,
    hangup: Hangup,
}

pub struct Injector {
    events: mpsc::Sender<Event>,
    shared: Arc<Shared>,
}

impl<G: Reconcile> Watcher<G> {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn subscribe(settings: ReloadSettings, groups: Vec<G>) -> Result<Self, ReloadError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn injected(settings: ReloadSettings, groups: Vec<G>) -> (Self, Injector) {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
    pub async fn run(self) -> G::Exit {
        todo!()
    }
}

impl Injector {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn changed(&self, path: &Path) {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
    pub fn rescan(&self) {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn fail(&self, kind: io::ErrorKind) {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
    pub fn hangup(&self) {
        todo!()
    }
}
