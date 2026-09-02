use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, OnceLock},
    task::{Context as TaskContext, Poll, Waker},
};

use tokio::sync::oneshot;
use tracing::{Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

use super::*;

#[tokio::test]
async fn typed_success_and_operation_error_are_preserved() {
    let success = process_fn(
        ProcessMetadata::non_interruptible("success-test"),
        || async { Ok::<_, io::Error>(42_u8) },
    );
    assert!(matches!(
        ProcessRun::execute(success, |_| {}).await,
        ProcessOutcome::Completed(Ok(42))
    ));

    let failure = process_fn(ProcessMetadata::non_interruptible("error-test"), || async {
        Err::<(), _>(io::Error::other("typed operation cause"))
    });
    match ProcessRun::execute(failure, |_| {}).await {
        ProcessOutcome::Completed(Err(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::Other);
            assert_eq!(error.to_string(), "typed operation cause");
        }
        _ => panic!("unexpected operation outcome"),
    }
}

#[tokio::test]
async fn panic_preserves_the_tokio_task_failure() {
    let process = process_fn(ProcessMetadata::non_interruptible("panic-test"), || async {
        panic!("deliberate process panic");
        #[allow(unreachable_code)]
        Ok::<_, io::Error>(())
    });

    let error = match ProcessRun::execute(process, |_| {}).await {
        ProcessOutcome::TaskFailed(error) => error,
        _ => panic!("unexpected panic outcome"),
    };
    assert!(error.is_panic());
    let payload = error.into_panic();
    assert_eq!(
        payload.downcast_ref::<&'static str>(),
        Some(&"deliberate process panic")
    );
}

#[derive(Default)]
struct CaptureState {
    records: Mutex<Vec<String>>,
    kinds: Mutex<HashMap<tracing::span::Id, String>>,
    terminal_waiters: Mutex<HashMap<String, oneshot::Sender<()>>>,
}

#[derive(Clone, Default)]
struct CaptureLayer(Arc<CaptureState>);

impl CaptureLayer {
    fn global() -> Self {
        static CAPTURE: OnceLock<CaptureLayer> = OnceLock::new();
        CAPTURE
            .get_or_init(|| {
                let capture = Self::default();
                let subscriber = tracing_subscriber::registry().with(capture.clone());
                tracing::subscriber::set_global_default(subscriber)
                    .expect("test subscriber installs once");
                capture
            })
            .clone()
    }

    fn terminal(&self, kind: &str) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        let previous = self
            .0
            .terminal_waiters
            .lock()
            .expect("terminal waiter lock")
            .insert(kind.to_owned(), sender);
        assert!(previous.is_none(), "one terminal waiter per process kind");
        receiver
    }
}

#[derive(Default)]
struct FieldCapture {
    rendered: String,
    process_kind: Option<String>,
}

impl Visit for FieldCapture {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.rendered.push_str(field.name());
        self.rendered.push('=');
        self.rendered.push_str(&format!("{value:?};"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "process.kind" {
            self.process_kind = Some(value.to_owned());
        }
        self.record_debug(field, &value);
    }
}

impl<Registry> Layer<Registry> for CaptureLayer
where
    Registry: Subscriber,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _context: Context<'_, Registry>,
    ) {
        let mut fields = FieldCapture::default();
        attributes.record(&mut fields);
        if let Some(kind) = fields.process_kind {
            self.0
                .kinds
                .lock()
                .expect("span kind lock")
                .insert(id.clone(), kind);
        }
        self.0
            .records
            .lock()
            .expect("capture layer lock")
            .push(format!(
                "{};{}",
                attributes.metadata().name(),
                fields.rendered
            ));
    }

    fn on_record(
        &self,
        span: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _context: Context<'_, Registry>,
    ) {
        let mut fields = FieldCapture::default();
        values.record(&mut fields);
        let terminal = fields.rendered.contains("process.outcome=");
        // A later record is prefixed with the span's fixed kind so a test can correlate a
        // terminal `process.outcome` with the node it belongs to.
        let kind = self
            .0
            .kinds
            .lock()
            .expect("span kind lock")
            .get(span)
            .cloned();
        let rendered = match &kind {
            Some(kind) => format!("process.kind={kind:?};{}", fields.rendered),
            None => fields.rendered,
        };
        self.0
            .records
            .lock()
            .expect("capture layer lock")
            .push(rendered);
        if !terminal {
            return;
        }
        if let Some(sender) = kind.and_then(|kind| {
            self.0
                .terminal_waiters
                .lock()
                .expect("terminal waiter lock")
                .remove(&kind)
        }) {
            assert!(sender.send(()).is_ok(), "terminal waiter remains");
        }
    }

    fn on_close(&self, id: tracing::span::Id, _context: Context<'_, Registry>) {
        self.0.kinds.lock().expect("span kind lock").remove(&id);
    }
}

#[tokio::test]
async fn trace_fields_are_fixed_and_payload_free() {
    let capture = CaptureLayer::global();

    let payload = "VERY_SECRET_PROCESS_PAYLOAD".to_owned();
    let process = process_fn(
        ProcessMetadata::non_interruptible("trace-test"),
        move || async move {
            assert_eq!(payload.len(), 27);
            Ok::<_, io::Error>(())
        },
    );
    assert!(matches!(
        ProcessRun::execute(process, |_| {}).await,
        ProcessOutcome::Completed(Ok(()))
    ));

    let trace = capture
        .0
        .records
        .lock()
        .expect("capture layer lock")
        .join("\n");
    assert!(trace.contains("process.run"), "{trace}");
    assert!(trace.contains("process.node"), "{trace}");
    assert!(trace.contains("run.id="), "{trace}");
    assert!(trace.contains("node.id="), "{trace}");
    assert!(trace.contains("parent.id=\"root\""), "{trace}");
    assert!(trace.contains("process.kind=\"trace-test\""), "{trace}");
    assert!(
        trace.contains("process.interruptibility=\"non-interruptible\""),
        "{trace}"
    );
    assert!(
        trace.contains("process.kind=\"trace-test\";process.outcome=\"succeeded\";"),
        "{trace}"
    );
    assert!(!trace.contains("VERY_SECRET_PROCESS_PAYLOAD"), "{trace}");

    let payload = "VERY_SECRET_CANCELLABLE_PAYLOAD".to_owned();
    let (handle, signal) = CancelSignal::pair();
    let cancellable = process_fn(
        ProcessMetadata::cancellable("trace-cancellable-test", signal),
        move || async move {
            assert_eq!(payload.len(), 31);
            Ok::<_, io::Error>(())
        },
    );
    assert!(matches!(
        ProcessRun::execute(cancellable, |_| {}).await,
        ProcessOutcome::Completed(Ok(()))
    ));
    drop(handle);

    let trace = capture
        .0
        .records
        .lock()
        .expect("capture layer lock")
        .join("\n");
    assert!(
        trace.contains(
            "process.kind=\"trace-cancellable-test\";process.interruptibility=\"cancellable\";"
        ),
        "{trace}"
    );
    assert!(
        trace.contains("process.kind=\"trace-cancellable-test\";process.outcome=\"succeeded\";"),
        "{trace}"
    );
    assert!(
        !trace.contains("VERY_SECRET_CANCELLABLE_PAYLOAD"),
        "{trace}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn admission_then_drop_delivers_exact_operation_error_to_observer() {
    let (started_sender, started_receiver) = oneshot::channel();
    let (observed_sender, observed_receiver) = oneshot::channel();
    let process = process_fn(
        ProcessMetadata::non_interruptible("admission-test"),
        move || async move {
            started_sender.send(()).expect("start observer remains");
            Err::<(), _>(io::Error::other("exact abandoned operation cause"))
        },
    );
    let mut execute = Box::pin(ProcessRun::execute(process, move |outcome| {
        assert!(
            observed_sender.send(outcome).is_ok(),
            "abandoned outcome observer remains"
        );
    }));

    // A current-thread runtime cannot poll the spawned supervisor until this task yields. One
    // manual poll therefore admits the supervisor and transfers process ownership, then returns
    // Pending on the outcome receiver while the process itself has not started yet.
    let waker = Waker::noop();
    let mut context = TaskContext::from_waker(waker);
    assert!(matches!(
        std::future::Future::poll(execute.as_mut(), &mut context),
        Poll::Pending
    ));
    drop(execute);

    started_receiver.await.expect("supervised process starts");
    match observed_receiver.await.expect("observer receives outcome") {
        ProcessOutcome::Completed(Err(error)) => {
            assert_eq!(error.to_string(), "exact abandoned operation cause");
        }
        _ => panic!("observer received the wrong abandoned outcome"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn queued_outcome_drop_delivers_exact_error_to_observer() {
    let capture = CaptureLayer::global();
    let terminal = capture.terminal("queued-outcome-test");
    let (started_sender, started_receiver) = oneshot::channel();
    let (observed_sender, observed_receiver) = oneshot::channel();
    let process = process_fn(
        ProcessMetadata::non_interruptible("queued-outcome-test"),
        move || async move {
            started_sender.send(()).expect("start observer remains");
            Err::<(), _>(io::Error::other("exact queued operation cause"))
        },
    );
    let mut execute = Box::pin(ProcessRun::execute(process, move |outcome| {
        assert!(
            observed_sender.send(outcome).is_ok(),
            "queued outcome observer remains"
        );
    }));
    let waker = Waker::noop();
    let mut context = TaskContext::from_waker(waker);
    assert!(matches!(
        std::future::Future::poll(execute.as_mut(), &mut context),
        Poll::Pending
    ));

    started_receiver.await.expect("supervised process starts");
    // The terminal trace record is made immediately before the envelope send. Because both run in
    // one supervisor poll on this current-thread runtime, observing this signal proves the outcome
    // is queued before this test regains control and drops the unpolled execute future.
    terminal.await.expect("supervisor records terminal outcome");
    drop(execute);

    match observed_receiver
        .await
        .expect("observer receives queued outcome")
    {
        ProcessOutcome::Completed(Err(error)) => {
            assert_eq!(error.to_string(), "exact queued operation cause");
        }
        _ => panic!("observer received the wrong queued outcome"),
    }
}

#[tokio::test]
async fn caller_abort_then_process_panic_delivers_raw_join_error_to_observer() {
    let (started_sender, started_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    let (observed_sender, observed_receiver) = oneshot::channel();
    let process = process_fn(
        ProcessMetadata::non_interruptible("abandoned-panic-test"),
        move || async move {
            started_sender.send(()).expect("start observer remains");
            release_receiver.await.expect("panic release remains");
            panic!("exact abandoned panic payload");
            #[allow(unreachable_code)]
            Ok::<_, io::Error>(())
        },
    );
    let caller = tokio::spawn(async move {
        ProcessRun::execute(process, move |outcome| {
            assert!(
                observed_sender.send(outcome).is_ok(),
                "abandoned panic observer remains"
            );
        })
        .await
    });
    started_receiver.await.expect("process starts");
    caller.abort();
    let caller_error = match caller.await {
        Err(error) => error,
        Ok(_) => panic!("outer execute caller completed after abort"),
    };
    assert!(caller_error.is_cancelled());

    release_sender
        .send(())
        .expect("surviving supervisor still owns the process");
    let error = match observed_receiver.await.expect("observer receives panic") {
        ProcessOutcome::TaskFailed(error) => error,
        _ => panic!("observer received the wrong abandoned panic outcome"),
    };
    assert!(error.is_panic());
    let payload = error.into_panic();
    assert_eq!(
        payload.downcast_ref::<&'static str>(),
        Some(&"exact abandoned panic payload")
    );
}

fn joined_trace(capture: &CaptureLayer) -> String {
    capture
        .0
        .records
        .lock()
        .expect("capture layer lock")
        .join("\n")
}

#[tokio::test]
async fn a_cancel_signal_aborts_a_cancellable_process_and_records_cancelled() {
    let capture = CaptureLayer::global();
    let terminal = capture.terminal("cancel-test");
    let (started_sender, started_receiver) = oneshot::channel();
    // The parked receiver's sender stays alive for the whole test, so the process can only leave
    // its await through the supervisor's abort.
    let (_park_sender, park_receiver) = oneshot::channel::<()>();
    let (handle, signal) = CancelSignal::pair();
    let process = process_fn(
        ProcessMetadata::cancellable("cancel-test", signal),
        move || async move {
            started_sender.send(()).expect("start observer remains");
            park_receiver
                .await
                .expect("the park sender outlives the process");
            Ok::<_, io::Error>(())
        },
    );
    let execute = tokio::spawn(ProcessRun::execute(process, |_| {}));

    started_receiver.await.expect("process starts");
    handle.cancel();
    handle.cancel();

    let error = match execute.await.expect("outer execute task joins") {
        ProcessOutcome::TaskFailed(error) => error,
        ProcessOutcome::Completed(_) => panic!("a parked process cannot complete"),
    };
    assert!(error.is_cancelled());
    assert!(!error.is_panic());
    terminal.await.expect("supervisor records terminal outcome");

    let trace = joined_trace(&capture);
    assert!(
        trace.contains("process.kind=\"cancel-test\";process.interruptibility=\"cancellable\";"),
        "{trace}"
    );
    assert!(
        trace.contains("process.kind=\"cancel-test\";process.outcome=\"cancelled\";"),
        "{trace}"
    );
}

#[tokio::test]
async fn a_completed_process_wins_over_a_late_signal() {
    let capture = CaptureLayer::global();
    let terminal = capture.terminal("late-signal-test");
    let (handle, signal) = CancelSignal::pair();
    let process = process_fn(
        ProcessMetadata::cancellable("late-signal-test", signal),
        || async { Ok::<_, io::Error>(7_u8) },
    );

    let outcome = ProcessRun::execute(process, |_| {}).await;
    handle.cancel();

    assert!(matches!(outcome, ProcessOutcome::Completed(Ok(7))));
    terminal.await.expect("supervisor records terminal outcome");
    let trace = joined_trace(&capture);
    assert!(
        trace.contains("process.kind=\"late-signal-test\";process.outcome=\"succeeded\";"),
        "{trace}"
    );
}

#[tokio::test]
async fn a_never_signal_leaves_a_cancellable_process_joined() {
    let capture = CaptureLayer::global();
    let terminal = capture.terminal("never-signal-test");
    let process = process_fn(
        ProcessMetadata::cancellable("never-signal-test", CancelSignal::never()),
        || async {
            // Yield so the supervisor observes the closed signal while the node still runs.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            Ok::<_, io::Error>("joined")
        },
    );

    assert!(matches!(
        ProcessRun::execute(process, |_| {}).await,
        ProcessOutcome::Completed(Ok("joined"))
    ));
    terminal.await.expect("supervisor records terminal outcome");
    let trace = joined_trace(&capture);
    assert!(
        trace.contains(
            "process.kind=\"never-signal-test\";process.interruptibility=\"cancellable\";"
        ),
        "{trace}"
    );
    assert!(
        trace.contains("process.kind=\"never-signal-test\";process.outcome=\"succeeded\";"),
        "{trace}"
    );
}

#[tokio::test]
async fn dropping_every_cancel_handle_does_not_cancel() {
    let (started_sender, started_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    let (handle, signal) = CancelSignal::pair();
    let second_handle = handle.clone();
    let process = process_fn(
        ProcessMetadata::cancellable("dropped-handle-test", signal),
        move || async move {
            started_sender.send(()).expect("start observer remains");
            release_receiver.await.expect("release sender remains");
            Ok::<_, io::Error>(())
        },
    );
    let execute = tokio::spawn(ProcessRun::execute(process, |_| {}));

    started_receiver.await.expect("process starts");
    drop(handle);
    drop(second_handle);
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(!execute.is_finished(), "a closed signal must not cancel");
    release_sender
        .send(())
        .expect("process still awaits release");

    assert!(matches!(
        execute.await.expect("outer execute task joins"),
        ProcessOutcome::Completed(Ok(()))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn an_abandoned_cancelled_outcome_reaches_the_observer() {
    let (started_sender, started_receiver) = oneshot::channel();
    let (_park_sender, park_receiver) = oneshot::channel::<()>();
    let (observed_sender, observed_receiver) = oneshot::channel();
    let (handle, signal) = CancelSignal::pair();
    let process = process_fn(
        ProcessMetadata::cancellable("abandoned-cancel-test", signal),
        move || async move {
            started_sender.send(()).expect("start observer remains");
            park_receiver
                .await
                .expect("the park sender outlives the process");
            Ok::<_, io::Error>(())
        },
    );
    let mut execute = Box::pin(ProcessRun::execute(process, move |outcome| {
        assert!(
            observed_sender.send(outcome).is_ok(),
            "abandoned cancel observer remains"
        );
    }));

    // As in the admission test: one manual poll admits the supervisor and transfers process
    // ownership before this outer future is dropped.
    let waker = Waker::noop();
    let mut context = TaskContext::from_waker(waker);
    assert!(matches!(
        std::future::Future::poll(execute.as_mut(), &mut context),
        Poll::Pending
    ));
    drop(execute);

    started_receiver.await.expect("supervised process starts");
    handle.cancel();
    let error = match observed_receiver.await.expect("observer receives outcome") {
        ProcessOutcome::TaskFailed(error) => error,
        ProcessOutcome::Completed(_) => panic!("observer received the wrong abandoned outcome"),
    };
    assert!(error.is_cancelled());
}
