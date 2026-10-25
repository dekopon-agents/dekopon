use super::*;

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
    let tree = TreeContext::new(Limits::default(), CallBudget::new(32)).with_cancel(vec![signal]);
    handle.cancel();
    assert_eq!(tree.charge_step(), Err(LimitExceeded::Cancelled));
    assert_eq!(tree.steps(), 0);
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
    let script = TreeContext::new(limits, CallBudget::new(32)).with_script_timer(Some(timer));
    let job = script
        .clone()
        .with_cancel(vec![CancelSignal::never()])
        .with_script_timer(None);
    tokio::time::advance(limits.timeout).await;
    assert_eq!(script.charge_step(), Err(LimitExceeded::Cancelled));
    assert!(!job.is_cancelled());
    assert_eq!(job.charge_step(), Ok(()));
    let expired_job = TreeContext::new(
        Limits {
            timeout: Duration::ZERO,
            ..limits
        },
        CallBudget::new(32),
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
        let tree = TreeContext::new(Limits::default(), CallBudget::new(32))
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
