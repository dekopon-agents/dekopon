//! The owned disk-to-upload boundary shared by outbound transports.

use dekopon_agent::attachment::GeneratedImage;
use dekopon_model::asset::BlobError;

use super::TransportError;

pub(super) struct HydratedImage {
    pub filename: String,
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
}

/// Transfer every lease before yielding, including those not read after an error. Only transient
/// buffers return to the async worker. Dropping the returned future does not abort the blocking
/// task: it still owns and disposes the leases when IO finishes. An unpolled transport future or
/// runtime shutdown before a queued task starts is not covered by this normal-disposal boundary.
pub(super) fn hydrate_images(
    images: Vec<GeneratedImage>,
) -> impl Future<Output = Result<Vec<HydratedImage>, TransportError>> + Send {
    hydrate_images_with(images, GeneratedImage::into_bytes)
}

fn hydrate_images_with(
    images: Vec<GeneratedImage>,
    mut read: impl FnMut(GeneratedImage) -> Result<Vec<u8>, BlobError> + Send + 'static,
) -> impl Future<Output = Result<Vec<HydratedImage>, TransportError>> + Send {
    let span = tracing::Span::current();
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let task = (!images.is_empty()).then(|| {
        tokio::task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                span.in_scope(move || {
                    images
                        .into_iter()
                        .enumerate()
                        .map(|(index, image)| {
                            let filename = image.filename(index);
                            let media_type = image.media_type();
                            let bytes = read(image)?;
                            Ok(HydratedImage {
                                filename,
                                media_type,
                                bytes,
                            })
                        })
                        .collect::<Result<Vec<_>, BlobError>>()
                })
            })
        })
    });
    async move {
        let Some(task) = task else {
            return Ok(Vec::new());
        };
        task.await
            .map_err(|error| {
                // Panic payloads may contain arbitrary text; retain the actionable join cause only.
                let reason = if error.is_panic() {
                    "panic"
                } else {
                    "cancelled"
                };
                tracing::error!(
                    reason,
                    "attachment hydration task failed; provider already executed"
                );
                TransportError::AttachmentTask { reason }
            })?
            .map_err(TransportError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_test_support::{CaptureLayer, Record};
    use std::{
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };
    use tracing_subscriber::prelude::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nprivate pixel sentinel";

    fn images() -> Vec<GeneratedImage> {
        (0..2)
            .map(|_| GeneratedImage::from_png(PNG.to_vec()).unwrap())
            .collect()
    }

    #[derive(Clone, Default)]
    struct SpoolThreads(Arc<Mutex<Vec<std::thread::ThreadId>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SpoolThreads {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _: &tracing::span::Id,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if attrs.metadata().name() == "asset.spool" {
                self.0.lock().unwrap().push(std::thread::current().id());
            }
        }
    }

    #[tokio::test]
    async fn hydration_preserves_bytes_trace_parentage_and_off_worker_disposal() {
        let capture = CaptureLayer::workspace();
        let threads = SpoolThreads::default();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .with(threads.clone())
            .set_default();
        let origin = tracing::info_span!("gateway.session");
        let images = origin.in_scope(images);
        threads.0.lock().unwrap().clear();
        let delivery = tracing::info_span!("transport.delivery");
        let result = delivery.in_scope(|| hydrate_images(images)).await.unwrap();
        for (index, image) in result.iter().enumerate() {
            assert_eq!(image.bytes, PNG);
            assert_eq!(image.media_type, "image/png");
            assert_eq!(
                image.filename,
                if index == 0 {
                    "generated-image.png"
                } else {
                    "generated-image-2.png"
                }
            );
        }
        let spool_threads = threads.0.lock().unwrap();
        assert_eq!(
            spool_threads.len(),
            4,
            "two reads and two final-owner cleanups"
        );
        assert!(
            spool_threads
                .iter()
                .all(|id| *id != std::thread::current().id())
        );
        for (operation, parent) in [
            ("read", "transport.delivery"),
            ("cleanup", "gateway.session"),
        ] {
            let spans = capture.records().into_iter().filter(|record| matches!(record,
                Record::Span { name: "asset.spool", fields, parent: actual }
                if fields.contains(&format!("operation=\"{operation}\"")) && actual.as_deref() == Some(parent)
            )).count();
            assert_eq!(spans, 2, "{}", capture.text());
        }
        assert!(!capture.text().contains("private pixel sentinel"));
        assert!(!capture.text().contains("dekopon-assets-"));
    }

    #[tokio::test]
    async fn slow_hydration_does_not_block_async_timers() {
        let (release, wait) = mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let async_thread = std::thread::current().id();
        let hydration = hydrate_images_with(images(), move |image| {
            assert_ne!(std::thread::current().id(), async_thread);
            if let Some(started) = started.take() {
                started.send(()).unwrap();
                // A bounded watchdog makes a broken boundary fail instead of hanging the suite.
                wait.recv_timeout(Duration::from_secs(2))
                    .expect("async timer must release the slow read");
            }
            image.into_bytes()
        });
        ready.await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        release.send(()).unwrap();
        assert_eq!(hydration.await.unwrap()[0].bytes, PNG);
    }

    #[tokio::test]
    async fn hydration_read_failure_keeps_cause_and_disposes_unread_images_off_worker() {
        let capture = CaptureLayer::workspace();
        let threads = SpoolThreads::default();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .with(threads.clone())
            .set_default();
        let images = images();
        threads.0.lock().unwrap().clear();
        let error = hydrate_images_with(images, |_image| {
            Err(BlobError::Io(std::io::ErrorKind::PermissionDenied))
        })
        .await
        .err()
        .unwrap();
        assert!(matches!(
            error,
            TransportError::Attachment(BlobError::Io(std::io::ErrorKind::PermissionDenied))
        ));
        assert!(error.to_string().contains("PermissionDenied"));
        let spool_threads = threads.0.lock().unwrap();
        assert_eq!(
            spool_threads.len(),
            2,
            "both owners cleaned up after first read refusal"
        );
        assert!(
            spool_threads
                .iter()
                .all(|id| *id != std::thread::current().id())
        );
    }

    #[tokio::test]
    async fn hydration_join_failure_is_distinct_without_rendering_panic_payload() {
        let error = hydrate_images_with(images(), |_image| panic!("private panic sentinel"))
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error,
            TransportError::AttachmentTask { reason: "panic" }
        ));
        assert_eq!(error.category(), "attachment-task");
        assert!(error.to_string().contains("provider already executed"));
        assert!(!error.to_string().contains("private panic sentinel"));
    }

    #[tokio::test]
    async fn dropping_hydration_wait_keeps_ownership_on_the_blocking_task() {
        let (release, wait) = mpsc::channel();
        let (finished, done) = tokio::sync::oneshot::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let mut finished = Some(finished);
        let async_thread = std::thread::current().id();
        let image = GeneratedImage::from_png(PNG.to_vec()).unwrap();
        let hydration = hydrate_images_with(vec![image], move |image| {
            assert_ne!(std::thread::current().id(), async_thread);
            started.take().unwrap().send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            let result = image.into_bytes(); // final owner is disposed before notifying the waiter
            finished.take().unwrap().send(()).unwrap();
            result
        });
        ready.await.unwrap();
        drop(hydration); // an unpolled wait can be cancelled even while its blocking read runs
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), done)
            .await
            .unwrap()
            .unwrap();
    }
}
