use std::{path::PathBuf, time::Duration};

use tokio::time::{Instant, MissedTickBehavior};

use crate::provider_manager;

pub(crate) const POLL: Duration = Duration::from_secs(2);
const WINDOW: u8 = 3;

#[derive(Clone, Debug)]
pub struct LoadedLock {
    pub(crate) path: PathBuf,
    pub(crate) digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockStop {
    Changed,
    Unsettled,
}

pub(crate) async fn watch(lock: &LoadedLock, uid: u32, period: Duration) -> LockStop {
    let mut interval = tokio::time::interval_at(Instant::now() + period, period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut observer = Observer::new(&lock.digest);
    loop {
        interval.tick().await;
        match observer.observe(provider_manager::lock_digest(&lock.path, uid).await) {
            Verdict::Watching => {}
            Verdict::Changed(current) => {
                tracing::warn!(
                    event = "broker_provider_lock_changed",
                    lock.loaded = %lock.digest,
                    lock.current = %current,
                    "provider lock changed; draining for a restart"
                );
                return LockStop::Changed;
            }
            Verdict::Unsettled(error) => {
                tracing::error!(
                    event = "broker_provider_lock_unsettled",
                    lock.loaded = %lock.digest,
                    polls = WINDOW,
                    cause = if error.is_some() { "unreadable" } else { "unstable" },
                    error = error.as_ref().map(|error| dekopon_core::error_chain(error)),
                    "provider lock did not settle; draining for a restart"
                );
                return LockStop::Unsettled;
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Verdict<E> {
    Watching,
    Changed(String),
    Unsettled(Option<E>),
}

struct Observer<'a> {
    loaded: &'a str,
    candidate: Option<String>,
    unsettled: u8,
}

impl<'a> Observer<'a> {
    const fn new(loaded: &'a str) -> Self {
        Self {
            loaded,
            candidate: None,
            unsettled: 0,
        }
    }

    fn observe<E>(&mut self, read: Result<String, E>) -> Verdict<E> {
        let failure = match read {
            Ok(digest) if digest == self.loaded => {
                self.candidate = None;
                self.unsettled = 0;
                return Verdict::Watching;
            }
            Ok(digest) if self.candidate.as_deref() == Some(digest.as_str()) => {
                return Verdict::Changed(digest);
            }
            Ok(digest) => {
                self.candidate = Some(digest);
                None
            }
            Err(error) => {
                self.candidate = None;
                Some(error)
            }
        };
        self.unsettled = self.unsettled.saturating_add(1);
        if self.unsettled < WINDOW {
            Verdict::Watching
        } else {
            Verdict::Unsettled(failure)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _};

    use super::*;

    fn read(digest: &str) -> Result<String, &'static str> {
        Ok(digest.to_owned())
    }

    #[test]
    fn identical_bytes_never_signal() {
        let mut observer = Observer::new("a");
        for _ in 0..10 {
            assert_eq!(observer.observe(read("a")), Verdict::Watching);
        }
    }

    #[test]
    fn a_replacement_signals_once_two_consecutive_reads_agree() {
        let mut observer = Observer::new("a");
        assert_eq!(observer.observe(read("b")), Verdict::Watching);
        assert_eq!(
            observer.observe(read("b")),
            Verdict::Changed("b".to_owned())
        );
    }

    #[test]
    fn an_unstable_read_inside_the_window_does_not_signal() {
        let mut observer = Observer::new("a");
        assert_eq!(observer.observe(read("b")), Verdict::Watching);
        assert_eq!(observer.observe(Err("torn")), Verdict::Watching);
        assert_eq!(observer.observe(read("a")), Verdict::Watching);
        assert_eq!(observer.observe(read("c")), Verdict::Watching);
        assert_eq!(observer.observe(read("a")), Verdict::Watching);
        assert_eq!(observer.observe(Err("torn")), Verdict::Watching);
        assert_eq!(observer.observe(read("a")), Verdict::Watching);
    }

    #[test]
    fn instability_past_the_window_fails_closed() {
        let mut observer = Observer::new("a");
        assert_eq!(observer.observe(read("b")), Verdict::Watching);
        assert_eq!(observer.observe(read("c")), Verdict::Watching);
        assert_eq!(observer.observe(read("d")), Verdict::Unsettled(None));

        let mut observer = Observer::new("a");
        assert_eq!(observer.observe(Err("gone")), Verdict::Watching);
        assert_eq!(observer.observe(Err("gone")), Verdict::Watching);
        assert_eq!(
            observer.observe(Err("gone")),
            Verdict::Unsettled(Some("gone"))
        );

        let mut observer = Observer::new("a");
        assert_eq!(observer.observe(read("b")), Verdict::Watching);
        assert_eq!(observer.observe(Err("torn")), Verdict::Watching);
        assert_eq!(observer.observe(read("b")), Verdict::Unsettled(None));
    }

    #[test]
    fn a_read_failure_then_a_stable_replacement_signals_the_replacement() {
        let mut observer = Observer::new("a");
        assert_eq!(observer.observe(Err("torn")), Verdict::Watching);
        assert_eq!(observer.observe(read("b")), Verdict::Watching);
        assert_eq!(
            observer.observe(read("b")),
            Verdict::Changed("b".to_owned())
        );
    }

    fn replace(path: &std::path::Path, contents: &[u8]) {
        let staged = path.with_extension("staged");
        fs::write(&staged, contents).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(&staged, path).unwrap();
    }

    const FAST: Duration = Duration::from_millis(20);

    #[tokio::test]
    async fn the_watcher_follows_the_file_it_loaded() {
        let uid = crate::current_uid();
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("providers.lock.yaml");
        replace(&path, b"providers: [a]\n");
        let lock = LoadedLock {
            digest: provider_manager::lock_digest(&path, uid).await.unwrap(),
            path,
        };

        replace(&lock.path, b"providers: [a]\n");
        assert!(
            tokio::time::timeout(FAST * 15, watch(&lock, uid, FAST))
                .await
                .is_err(),
            "an identical atomic rewrite must not signal"
        );

        replace(&lock.path, b"providers: [b]\n");
        assert_eq!(
            tokio::time::timeout(FAST * 15, watch(&lock, uid, FAST))
                .await
                .expect("an atomic replacement signals"),
            LockStop::Changed
        );

        fs::remove_file(&lock.path).unwrap();
        assert_eq!(
            tokio::time::timeout(FAST * 15, watch(&lock, uid, FAST))
                .await
                .expect("a vanished lock fails closed"),
            LockStop::Unsettled
        );
    }
}
