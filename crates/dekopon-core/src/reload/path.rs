use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use super::ReloadError;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum WatchedPath {
    File { path: PathBuf },
    Directory { directory: PathBuf },
    Tree { root: PathBuf },
    Projection { directory: PathBuf },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Registration {
    pub(super) directory: PathBuf,
    pub(super) depth: Depth,
}

#[expect(dead_code, reason = "skeleton: daemon/watcher-core")]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Depth {
    Flat,
    Recursive,
}

impl WatchedPath {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn file(path: &Path) -> Result<Self, ReloadError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn directory(path: &Path) -> Result<Self, ReloadError> {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn tree(path: &Path) -> Result<Self, ReloadError> {
        todo!()
    }

    #[expect(clippy::todo, dead_code, reason = "skeleton: daemon/watcher-core")]
    pub(super) fn registration(&self) -> Registration {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        dead_code,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub(super) fn covers(&self, changed: &Path) -> bool {
        todo!()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WatchSet(BTreeSet<WatchedPath>);

impl WatchSet {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn insert(&mut self, path: WatchedPath) {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        dead_code,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub(super) fn covers(&self, changed: &Path) -> bool {
        todo!()
    }

    #[expect(clippy::todo, dead_code, reason = "skeleton: daemon/watcher-core")]
    pub(super) fn registrations(&self) -> BTreeSet<Registration> {
        todo!()
    }
}
