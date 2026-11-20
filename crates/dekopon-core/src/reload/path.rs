use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use super::ReloadError;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum WatchedPath {
    File { path: PathBuf },
    Tree { root: PathBuf },
    ProjectedVolume { directory: PathBuf },
    LiveDirectory { directory: PathBuf },
}

impl WatchedPath {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn classify(path: &Path) -> Result<Self, ReloadError> {
        todo!()
    }

    #[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
    pub fn registration(&self) -> &Path {
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
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn covers(&self, changed: &Path) -> bool {
        todo!()
    }

    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    pub fn delta<'a>(&'a self, next: &'a Self) -> (Vec<&'a WatchedPath>, Vec<&'a WatchedPath>) {
        todo!()
    }
}
