#![expect(dead_code, reason = "skeleton: daemon/watcher-core")]

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

use tokio::sync::mpsc;

use super::{GroupId, Reconcile, ReloadError, ReloadSettings, Stop, WatchSet};

// A full queue sets `overflowed` instead of blocking notify's thread; the run loop turns it into
// one rescan of every group, so a burst coalesces and no change is lost.
const EVENT_QUEUE: usize = 64;

enum Event {
    Changed(PathBuf),
    Rescan,
    Failed(io::ErrorKind),
    Hangup,
}

enum Backend {
    Notify(notify::RecommendedWatcher),
    Injected,
}

enum Hangup {
    Signal(tokio::signal::unix::Signal),
    Injected,
}

pub struct Watcher<G: Reconcile> {
    settings: ReloadSettings,
    groups: Vec<(GroupId, G, WatchSet)>,
    backend: Backend,
    events: mpsc::Receiver<Event>,
    overflowed: Arc<AtomicBool>,
    hangup: Hangup,
}

pub struct Injector(mpsc::Sender<Event>);

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
    pub async fn run(self) -> Stop<G::Exit> {
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
