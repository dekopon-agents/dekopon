use super::*;
use std::{sync::LazyLock, time::Duration as StdDuration};

pub(crate) fn threads() -> ShellThreads {
    static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    });
    ShellThreads::new(4, RUNTIME.handle().clone())
}

const PROBE: ThreadName = ThreadName::new("sh-thread-probe");

#[test]
fn a_shell_thread_carries_its_full_name_and_blocks_on_a_runtime_timer() {
    let threads = threads();
    let [permit] = threads.reserve().unwrap();
    let (name, slept) = std::thread::scope(|scope| {
        permit
            .spawn(scope, PROBE, || {
                let name = std::thread::current().name().map(str::to_owned);
                let slept = tokio::runtime::Handle::current().block_on(async {
                    tokio::time::sleep(StdDuration::from_millis(1)).await;
                    true
                });
                (name, slept)
            })
            .unwrap()
            .join()
            .unwrap()
    });
    assert_eq!(name.as_deref(), Some("sh-thread-probe"));
    assert!(slept);
}

#[cfg(target_os = "linux")]
#[test]
fn the_kernel_reports_a_shell_thread_name_untruncated() {
    let threads = threads();
    let [permit] = threads.reserve().unwrap();
    let comm = std::thread::scope(|scope| {
        permit
            .spawn(scope, PROBE, || {
                std::fs::read_to_string("/proc/thread-self/comm")
            })
            .unwrap()
            .join()
            .unwrap()
            .unwrap()
    });
    assert_eq!(comm.trim_end(), "sh-thread-probe");
}

#[test]
fn a_group_past_the_cap_is_refused_whole_and_takes_no_permit() {
    let threads = threads();
    let held = threads.reserve_many(3).unwrap();
    assert_eq!(
        threads.reserve::<2>().map(|_| ()),
        Err(LimitExceeded::ShellThreads { maximum: 4 })
    );
    let [last] = threads.reserve().unwrap();
    drop((held, last));
    assert_eq!(threads.reserve_many(4).map(|permits| permits.len()), Ok(4));
}

#[test]
fn a_panicking_shell_thread_returns_its_permit() {
    let threads = threads();
    let permits = threads.reserve_many(4).unwrap();
    let panicked = std::thread::scope(|scope| {
        let handles = permits
            .into_iter()
            .map(|permit| {
                permit
                    .spawn(scope, PROBE, || panic!("stage panicked"))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        handles.into_iter().all(|handle| handle.join().is_err())
    });
    assert!(panicked);
    assert_eq!(threads.reserve_many(4).map(|permits| permits.len()), Ok(4));
}

#[test]
fn an_unspawned_permit_returns_to_the_budget() {
    let threads = threads();
    drop(threads.reserve::<4>().unwrap());
    assert_eq!(threads.reserve_many(4).map(|permits| permits.len()), Ok(4));
}

#[tokio::test]
async fn a_request_completes_current_and_future_waits() {
    let (handle, signal) = CancelSignal::pair();
    let waiting = signal.cancelled();
    tokio::pin!(waiting);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    handle.cancel();
    waiting.await;
    drop(handle);
    signal.cancelled().await;
}

#[tokio::test]
async fn dropping_every_handle_leaves_the_wait_pending() {
    let (handle, signal) = CancelSignal::pair();
    let second = handle.clone();
    drop(handle);
    drop(second);
    let waiting = signal.cancelled();
    tokio::pin!(waiting);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
}

#[tokio::test]
async fn a_never_signal_leaves_the_wait_pending() {
    let signal = CancelSignal::never();
    let waiting = signal.cancelled();
    tokio::pin!(waiting);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
}

#[test]
fn a_synchronous_boundary_reads_the_request_without_awaiting_it() {
    let (handle, signal) = CancelSignal::pair();
    assert!(!signal.is_cancelled());
    handle.cancel();
    assert!(signal.is_cancelled());
    assert!(signal.is_cancelled(), "a request never lapses");
    drop(handle);
    assert!(signal.is_cancelled());
    assert!(!CancelSignal::never().is_cancelled());
}

#[test]
fn a_cancelled_tree_makes_the_next_step_charge_return_cancelled_without_an_invoker() {
    let (handle, signal) = CancelSignal::pair();
    let tree = TreeContext::new(Limits::default(), CallBudget::new(32), threads())
        .with_cancel(vec![signal]);
    handle.cancel();
    assert_eq!(tree.charge_step(), Err(LimitExceeded::Cancelled));
    assert_eq!(tree.steps(), 0);
}

#[tokio::test(start_paused = true)]
async fn an_elapsed_script_timer_reports_a_deadline_and_a_cancelled_parent_reports_cancellation() {
    let limits = Limits {
        timeout: Duration::from_secs(5),
        ..Limits::default()
    };
    let mut timer = Box::pin(tokio::time::sleep(limits.timeout));
    timer.as_mut().await;
    let tree =
        TreeContext::new(limits, CallBudget::new(32), threads()).with_script_timer(Some(timer));
    let deadline = Err(LimitExceeded::Deadline { timeout_ms: 5_000 });

    assert!(!tree.is_cancelled());
    assert_eq!(tree.charge_step(), deadline);
    assert_eq!(tree.check_deadline(), deadline);

    let (parent, signal) = CancelSignal::pair();
    let tree = tree.with_parent_cancel(signal);
    parent.cancel();
    assert!(tree.is_cancelled());
    assert_eq!(tree.charge_step(), Err(LimitExceeded::Cancelled));
}

#[tokio::test(start_paused = true)]
async fn script_deadline_cancellation_does_not_change_a_jobs_deadline_error() {
    let limits = Limits {
        timeout: Duration::from_secs(5),
        ..Limits::default()
    };
    let mut timer = Box::pin(tokio::time::sleep(limits.timeout));
    assert!(
        std::future::Future::poll(
            timer.as_mut(),
            &mut std::task::Context::from_waker(std::task::Waker::noop()),
        )
        .is_pending()
    );
    let script =
        TreeContext::new(limits, CallBudget::new(32), threads()).with_script_timer(Some(timer));
    let job = script
        .clone()
        .with_cancel(vec![CancelSignal::never()])
        .with_script_timer(None);
    tokio::time::advance(limits.timeout).await;
    assert_eq!(
        script.charge_step(),
        Err(LimitExceeded::Deadline { timeout_ms: 5_000 })
    );
    assert!(!job.is_cancelled());
    assert_eq!(job.charge_step(), Ok(()));
    let expired_job = TreeContext::new(
        Limits {
            timeout: Duration::ZERO,
            ..limits
        },
        CallBudget::new(32),
        threads(),
    );
    assert_eq!(
        expired_job.check_deadline(),
        Err(LimitExceeded::Deadline { timeout_ms: 0 })
    );
}

#[test]
fn a_child_tree_keeps_both_its_starter_and_broker_cancellation_sources() {
    for cancel_parent in [false, true] {
        let (starter, starter_signal) = CancelSignal::pair();
        let (parent, parent_signal) = CancelSignal::pair();
        let tree = TreeContext::new(Limits::default(), CallBudget::new(32), threads())
            .with_cancel(vec![starter_signal])
            .with_parent_cancel(parent_signal);
        if cancel_parent {
            parent.cancel();
        } else {
            starter.cancel();
        }
        assert_eq!(tree.charge_step(), Err(LimitExceeded::Cancelled));
    }
}

struct ThreadTeardown {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
    timed_out: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for ThreadTeardown {
    fn drop(&mut self) {
        self.entered.take().unwrap().send(()).unwrap_or(());
        self.timed_out.store(
            self.release
                .recv_timeout(StdDuration::from_secs(5))
                .is_err(),
            std::sync::atomic::Ordering::Release,
        );
    }
}

thread_local! {
    static THREAD_TEARDOWN: std::cell::RefCell<Option<ThreadTeardown>> = const { std::cell::RefCell::new(None) };
}

#[tokio::test]
async fn owned_thread_tls_teardown_keeps_the_async_worker_free_until_joined() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let threads = threads();
    let [permit] = threads.reserve().unwrap();
    let (entered, teardown) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let timed_out = Arc::new(AtomicBool::new(false));
    let expired = Arc::clone(&timed_out);
    let join = permit
        .spawn_owned(PROBE, move || {
            THREAD_TEARDOWN.with(|slot| {
                *slot.borrow_mut() = Some(ThreadTeardown {
                    entered: Some(entered),
                    release: released,
                    timed_out: expired,
                })
            });
            let slept = tokio::runtime::Handle::current().block_on(async {
                tokio::time::sleep(StdDuration::from_millis(1)).await;
                true
            });
            (std::thread::current().name().map(str::to_owned), slept)
        })
        .unwrap();
    let mut owned = tokio::task::JoinSet::new();
    owned.spawn(join);
    teardown.await.unwrap();
    tokio::task::yield_now().await;
    assert!(
        owned.try_join_next().is_none(),
        "TLS still owns the native thread"
    );
    assert_eq!(
        threads.reserve::<4>().map(|_| ()),
        Err(LimitExceeded::ShellThreads { maximum: 4 }),
        "TLS teardown still holds its permit"
    );
    let progressed = !timed_out.load(Ordering::Acquire);
    release.send(()).unwrap_or(());
    let (name, slept) = owned.join_next().await.unwrap().unwrap().unwrap().unwrap();
    assert!(
        progressed,
        "a synchronous join stalled the only async worker in TLS teardown"
    );
    assert!(!timed_out.load(Ordering::Acquire));
    assert_eq!(name.as_deref(), Some("sh-thread-probe"));
    assert!(slept);
}

#[test]
fn a_long_owned_thread_leaves_the_only_blocking_slot_free_for_short_io() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let threads = ShellThreads::new(1, runtime.handle().clone());
    let timed_out = Arc::new(AtomicBool::new(false));
    let expired = Arc::clone(&timed_out);
    runtime.block_on(async {
        let [permit] = threads.reserve().unwrap();
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let join = permit
            .spawn_owned(PROBE, move || {
                started.send(()).unwrap_or(());
                expired.store(
                    released.recv_timeout(StdDuration::from_secs(5)).is_err(),
                    Ordering::Release,
                );
            })
            .unwrap();
        let mut owned = tokio::task::JoinSet::new();
        owned.spawn(join);
        running.await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(tokio::task::spawn_blocking(|| 42).await.unwrap(), 42);
        release.send(()).unwrap_or(());
        owned.join_next().await.unwrap().unwrap().unwrap().unwrap();
    });
    assert!(
        !timed_out.load(Ordering::Acquire),
        "the join occupied the blocking pool before the body completed"
    );
    assert!(threads.reserve::<1>().is_ok());
}
