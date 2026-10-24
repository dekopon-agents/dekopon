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
