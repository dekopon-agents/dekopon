use std::{
    io,
    sync::Arc,
    thread::{self, Scope, ScopedJoinHandle},
};

use tokio::{
    runtime::Handle,
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
};

use crate::LimitExceeded;

#[derive(Clone, Debug)]
pub struct ShellThreads {
    permits: Arc<Semaphore>,
    maximum: u32,
    runtime: Handle,
}

impl ShellThreads {
    pub const STACK_BYTES: usize = 2 * 1024 * 1024;

    #[must_use]
    pub fn new(maximum: u32, runtime: Handle) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(maximum as usize)),
            maximum,
            runtime,
        }
    }

    pub fn reserve<const N: usize>(&self) -> Result<[ThreadPermit; N], LimitExceeded> {
        self.reserve_many(N)?
            .try_into()
            .map_err(|_unsplit: Vec<ThreadPermit>| self.exhausted())
    }

    /// Takes every permit or none, so a group of cooperating threads never starts half admitted.
    pub fn reserve_many(&self, count: usize) -> Result<Vec<ThreadPermit>, LimitExceeded> {
        let mut permits = Vec::with_capacity(count);
        let Some(last) = count.checked_sub(1) else {
            return Ok(permits);
        };
        let requested = u32::try_from(count).map_err(|_oversized| self.exhausted())?;
        let mut group = Arc::clone(&self.permits)
            .try_acquire_many_owned(requested)
            .map_err(|_busy| self.exhausted())?;
        for _ in 0..last {
            let one = group.split(1).ok_or_else(|| self.exhausted())?;
            permits.push(self.permit(one));
        }
        permits.push(self.permit(group));
        Ok(permits)
    }

    fn permit(&self, permit: OwnedSemaphorePermit) -> ThreadPermit {
        ThreadPermit {
            permit,
            runtime: self.runtime.clone(),
        }
    }

    const fn exhausted(&self) -> LimitExceeded {
        LimitExceeded::ShellThreads {
            maximum: self.maximum,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadName(&'static str);

impl ThreadName {
    /// Linux keeps only the first 15 bytes of a thread name, so a `const` longer name fails to build.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        assert!(name.len() <= 15, "a thread name is at most 15 bytes");
        Self(name)
    }
}

#[derive(Debug)]
#[must_use]
pub struct ThreadPermit {
    permit: OwnedSemaphorePermit,
    runtime: Handle,
}

struct Completed(Option<oneshot::Sender<()>>);

impl Drop for Completed {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            done.send(()).unwrap_or(());
        }
    }
}

impl ThreadPermit {
    pub fn spawn_owned<T: Send + 'static>(
        self,
        name: ThreadName,
        run: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<impl Future<Output = Result<thread::Result<T>, tokio::task::JoinError>> + Send>
    {
        let Self { permit, runtime } = self;
        let (done, completed) = oneshot::channel();
        #[expect(
            clippy::disallowed_methods,
            reason = "owner: the returned join operation, retained by the gateway; bound: the ShellThreads permit moved into the thread"
        )]
        let thread = thread::Builder::new()
            .name(name.0.to_owned())
            .stack_size(ShellThreads::STACK_BYTES)
            .spawn(move || {
                let _completed = Completed(Some(done));
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _runtime = runtime.enter();
                    run()
                }));
                (permit, result)
            })?;
        Ok(async move {
            drop(completed.await);
            // Closure completion precedes TLS destruction, so even the final join stays off workers.
            tokio::task::spawn_blocking(move || match thread.join() {
                Ok((_permit, result)) => result,
                Err(payload) => Err(payload),
            })
            .await
        })
    }

    /// The permit lives as long as the thread, so it is released on return, unwind or a refused spawn.
    pub fn spawn<'scope, T: Send + 'scope>(
        self,
        scope: &'scope Scope<'scope, '_>,
        name: ThreadName,
        run: impl FnOnce() -> T + Send + 'scope,
    ) -> io::Result<ScopedJoinHandle<'scope, T>> {
        let Self { permit, runtime } = self;
        #[expect(
            clippy::disallowed_methods,
            reason = "owner: the caller's thread::scope joins it; bound: the ShellThreads permit moved into the thread"
        )]
        thread::Builder::new()
            .name(name.0.to_owned())
            .stack_size(ShellThreads::STACK_BYTES)
            .spawn_scoped(scope, move || {
                let _permit = permit;
                let _runtime = runtime.enter();
                run()
            })
    }
}
