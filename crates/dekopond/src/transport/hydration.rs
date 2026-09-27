use std::collections::VecDeque;

use dekopon_agent::attachment::GeneratedImage;
use dekopon_model::asset::BlobError;

use super::TransportError;

pub(super) struct HydratedImage {
    pub filename: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// Dropping the returned future does not abort the blocking task, which still disposes its leases
/// when IO finishes; an unpolled future is not covered by this guarantee.
pub(super) fn hydrate_images(
    images: Vec<GeneratedImage>,
) -> impl Future<Output = Result<Vec<HydratedImage>, TransportError>> + Send {
    hydrate_images_with(0, images, GeneratedImage::into_bytes)
}

/// Reading everything up front would hold all attachments resident at once, so single-attachment
/// transports read one at a time; only Discord's multipart post reads all at once.
#[derive(Default)]
pub(super) struct ImageQueue {
    images: VecDeque<GeneratedImage>,
    next: usize,
}

impl ImageQueue {
    pub(super) fn new(images: Vec<GeneratedImage>) -> Self {
        Self {
            images: images.into(),
            next: 0,
        }
    }

    pub(super) async fn decoded_lengths(&mut self) -> Result<Vec<usize>, TransportError> {
        let taken = std::mem::take(&mut self.images);
        let span = tracing::Span::current();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let task = tokio::task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                span.in_scope(move || {
                    let lengths = taken
                        .iter()
                        .map(GeneratedImage::decoded_len)
                        .collect::<Result<Vec<_>, BlobError>>();
                    (taken, lengths)
                })
            })
        });
        let (taken, lengths) = join(task.await)?;
        self.images = taken;
        lengths.map_err(TransportError::from)
    }

    pub(super) fn filenames(&self) -> Vec<String> {
        self.images
            .iter()
            .enumerate()
            .map(|(index, image)| image.filename(self.next + index))
            .collect()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub(super) async fn next(&mut self) -> Option<Result<(usize, HydratedImage), TransportError>> {
        let image = self.images.pop_front()?;
        let index = self.next;
        self.next += 1;
        Some(
            hydrate_images_with(index, vec![image], GeneratedImage::into_bytes)
                .await
                .map(|mut read| (index, read.remove(0))),
        )
    }

    pub(super) async fn read_all(&mut self) -> Result<Vec<HydratedImage>, TransportError> {
        let taken = std::mem::take(&mut self.images);
        let first = self.next;
        let span = tracing::Span::current();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let task = tokio::task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                span.in_scope(move || {
                    let read = taken
                        .iter()
                        .enumerate()
                        .map(|(index, image)| {
                            Ok(HydratedImage {
                                filename: image.filename(first + index),
                                media_type: image.media_type().to_owned(),
                                bytes: image.bytes()?,
                            })
                        })
                        .collect::<Result<Vec<_>, BlobError>>();
                    (taken, read)
                })
            })
        });
        let (taken, read) = join(task.await)?;
        self.images = taken;
        read.map_err(TransportError::from)
    }
}

fn join<T>(joined: Result<T, tokio::task::JoinError>) -> Result<T, TransportError> {
    joined.map_err(|error| {
        // Panic payloads may contain arbitrary text, so only the fixed reason string is kept, never
        // the payload itself.
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
    })
}

fn hydrate_images_with(
    first_index: usize,
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
                            let filename = image.filename(first_index + index);
                            let media_type = image.media_type().to_owned();
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
        join(task.await)?.map_err(TransportError::from)
    }
}

pub(super) enum AcceptedTypes {
    Files,
    Photos,
}
pub(super) fn validate_types(
    images: &[GeneratedImage],
    accepted: AcceptedTypes,
) -> Result<(), TransportError> {
    for image in images {
        let label = image.media_type();
        let valid =
            !label.contains('*') && reqwest::multipart::Part::text("").mime_str(label).is_ok();
        let allowed = match accepted {
            AcceptedTypes::Files => valid,
            AcceptedTypes::Photos => valid && matches!(label, "image/png" | "image/jpeg"),
        };
        if !allowed {
            let accepted = match accepted {
                AcceptedTypes::Files => "any concrete valid media type",
                AcceptedTypes::Photos => "image/png, image/jpeg",
            };
            return Err(TransportError::AssetType { accepted });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn file_and_photo_adapters_refuse_unsupported_labels_before_hydration() {
        use super::{AcceptedTypes, validate_types};
        use dekopon_broker_protocol::AssetEncoding;
        use dekopon_model::asset::DiskBlob;
        for label in [
            "image/png",
            "image/jpeg",
            "image/webp",
            "application/pdf",
            "text/plain",
            "application/octet-stream",
            "",
            "image/*",
            "not a mime",
        ] {
            let images = vec![dekopon_agent::attachment::GeneratedImage::new(
                DiskBlob::from_bytes(b"bounded").unwrap(),
                label.to_owned(),
                AssetEncoding::Identity,
            )];
            for (adapter, allowed, expected) in [
                (
                    AcceptedTypes::Files,
                    !matches!(label, "" | "image/*" | "not a mime"),
                    "any concrete valid media type",
                ),
                (
                    AcceptedTypes::Photos,
                    matches!(label, "image/png" | "image/jpeg"),
                    "image/png, image/jpeg",
                ),
            ] {
                match validate_types(&images, adapter) {
                    Ok(()) => assert!(allowed, "{label} must be refused"),
                    Err(TransportError::AssetType { accepted }) => {
                        assert!(!allowed, "{label} must be accepted");
                        assert_eq!(accepted, expected);
                    }
                    Err(error) => panic!("expected AssetType for {label}, got {error:?}"),
                }
            }
        }
    }

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
                    "asset-1.png"
                } else {
                    "asset-2.png"
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
            ("cleanup", "transport.delivery"),
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

    fn reads(capture: &CaptureLayer) -> usize {
        capture
            .records()
            .into_iter()
            .filter(|record| {
                matches!(record, Record::Span { name: "asset.spool", fields, .. }
                    if fields.contains(&"operation=\"read\"".to_owned()))
            })
            .count()
    }

    #[tokio::test]
    async fn a_queue_reads_one_image_at_a_time() {
        let capture = CaptureLayer::workspace();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();
        let mut queue = ImageQueue::new(images());
        assert_eq!(queue.decoded_lengths().await.unwrap(), vec![PNG.len(); 2]);
        assert!(!queue.is_empty());

        let (index, first) = queue.next().await.unwrap().unwrap();

        assert_eq!(index, 0);
        assert_eq!(first.filename, "asset-1.png");
        assert_eq!(first.bytes, PNG);
        assert_eq!(
            reads(&capture),
            1,
            "an image was read before its own upload asked for it"
        );

        let (index, second) = queue.next().await.unwrap().unwrap();

        assert_eq!(index, 1);
        assert_eq!(second.filename, "asset-2.png");
        assert_eq!(reads(&capture), 2);
        assert!(queue.next().await.is_none());
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn a_queue_dropped_part_way_disposes_what_is_left() {
        let threads = SpoolThreads::default();
        let _guard = tracing_subscriber::registry()
            .with(threads.clone())
            .set_default();
        let mut queue = ImageQueue::new(images());
        threads.0.lock().unwrap().clear();

        drop(queue.next().await.unwrap().unwrap());
        drop(queue);

        let spool_threads = threads.0.lock().unwrap();
        assert_eq!(
            spool_threads.len(),
            3,
            "one read and both final-owner cleanups"
        );
        assert_ne!(
            spool_threads[0],
            std::thread::current().id(),
            "reads stay off the worker"
        );
        assert_eq!(
            spool_threads[2],
            std::thread::current().id(),
            "the rest drops inline"
        );
    }

    #[tokio::test]
    async fn reading_images_hands_the_leases_back_for_a_second_read() {
        let capture = CaptureLayer::workspace();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();

        let mut queue = ImageQueue::new(images());
        let first = queue.read_all().await.unwrap();
        let second = queue.read_all().await.unwrap();

        assert_eq!(reads(&capture), 4, "two images read twice");
        assert!(!queue.is_empty(), "the leases did not come back");
        for read in [first, second] {
            assert_eq!(read.len(), 2);
            assert_eq!(read[0].filename, "asset-1.png");
            assert_eq!(read[1].filename, "asset-2.png");
            assert!(read.iter().all(|image| image.bytes == PNG));
        }
    }

    #[tokio::test]
    async fn slow_hydration_does_not_block_async_timers() {
        let (release, wait) = mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        let async_thread = std::thread::current().id();
        let hydration = hydrate_images_with(0, images(), move |image: GeneratedImage| {
            assert_ne!(std::thread::current().id(), async_thread);
            if let Some(started) = started.take() {
                started.send(()).unwrap();
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
        let error = hydrate_images_with(0, images, |_image| {
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
        let error = hydrate_images_with(0, images(), |_image| panic!("private panic sentinel"))
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
        let hydration = hydrate_images_with(0, vec![image], move |image| {
            assert_ne!(std::thread::current().id(), async_thread);
            started.take().unwrap().send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            let result = image.into_bytes();
            finished.take().unwrap().send(()).unwrap();
            result
        });
        ready.await.unwrap();
        drop(hydration);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), done)
            .await
            .unwrap()
            .unwrap();
    }
}
