use dekopon_broker_protocol::{
    AssetEncoding, AssetRow, InvokeAssets, MAX_ASSET_DESCRIPTORS, NewAsset,
};
use dekopon_core::base64::{DecoderReader, STANDARD};
use dekopon_model::asset::{BlobError, DiskBlob};
use parking_lot::Mutex;
use std::{fmt, future::Future, io::Read, os::fd::OwnedFd, pin::Pin, sync::Arc};
use thiserror::Error;

pub use dekopon_model::asset::MAX_ATTACHMENT_BYTES;
pub const MAX_INVOCATION_ASSET_BYTES: usize = dekopon_core::asset::MAX_DECODED_INVOCATION_BYTES;

pub struct GeneratedImage {
    data: DiskBlob,
    content_type: String,
    encoding: AssetEncoding,
}
impl GeneratedImage {
    pub fn new(data: DiskBlob, content_type: String, encoding: AssetEncoding) -> Self {
        Self {
            data,
            content_type,
            encoding,
        }
    }
    pub fn from_png(data: Vec<u8>) -> Result<Self, BlobError> {
        Ok(Self::new(
            DiskBlob::from_bytes(&data)?,
            "image/png".to_owned(),
            AssetEncoding::Identity,
        ))
    }
    pub fn media_type(&self) -> &str {
        &self.content_type
    }
    pub fn filename(&self, index: usize) -> String {
        let content_type = self
            .content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let extension = match content_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "video/mp4" => "mp4",
            "text/plain" => "txt",
            "application/pdf" => "pdf",
            "text/markdown" => "md",
            "text/csv" => "csv",
            "application/json" => "json",
            "text/html" => "html",
            "text/xml" | "application/xml" => "xml",
            "application/msword" => "doc",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
            "application/rtf" => "rtf",
            other if other.starts_with("text/") => "txt",
            _ => "bin",
        };
        format!("asset-{}.{extension}", index + 1)
    }
    pub fn bytes(&self) -> Result<Vec<u8>, BlobError> {
        match self.encoding {
            AssetEncoding::Identity => self.data.read(),
            AssetEncoding::Base64 => {
                let start = std::time::Instant::now();
                let span = tracing::info_span!(
                    "asset.decode",
                    bytes = self.data.len(),
                    duration_us = tracing::field::Empty
                );
                span.in_scope(|| {
                    let len = self.decoded_len()?;
                    let mut decoded = vec![0; len];
                    DecoderReader::new(
                        BlobReader {
                            blob: &self.data,
                            offset: 0,
                        },
                        &STANDARD,
                    )
                    .read_exact(&mut decoded)?;
                    span.record("duration_us", start.elapsed().as_micros() as u64);
                    Ok(decoded)
                })
            }
        }
    }
    /// This does blocking I/O and must be called off the async worker thread, the same as bytes().
    pub fn decoded_len(&self) -> Result<usize, BlobError> {
        match self.encoding {
            AssetEncoding::Identity => Ok(self.data.len()),
            AssetEncoding::Base64 => {
                let mut tail = [0; 2];
                if !self.data.is_empty() {
                    let offset = self
                        .data
                        .len()
                        .checked_sub(tail.len())
                        .ok_or(BlobError::Io(std::io::ErrorKind::InvalidData))?;
                    self.data.read_exact_at(&mut tail, offset as u64)?;
                }
                dekopon_core::base64::decoded_len(self.data.len() as u64, &tail)
                    .map(|len| len as usize) // decoded bytes cannot exceed the bounded stored size
                    .map_err(|error| match error {
                        dekopon_core::base64::CodecError::TooLarge => BlobError::TooLarge,
                        dekopon_core::base64::CodecError::InvalidEncoding => {
                            BlobError::Io(std::io::ErrorKind::InvalidData)
                        }
                    })
            }
        }
    }
    pub fn len(&self) -> usize {
        self.data.len()
    }
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    pub fn into_bytes(self) -> Result<Vec<u8>, BlobError> {
        self.bytes()
    }
}
impl fmt::Debug for GeneratedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeneratedImage")
            .field("content_type", &self.content_type)
            .field("bytes", &self.len())
            .finish()
    }
}
struct BlobReader<'a> {
    blob: &'a DiskBlob,
    offset: usize,
}
impl Read for BlobReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let count = out.len().min(self.blob.len().saturating_sub(self.offset));
        self.blob
            .read_exact_at(&mut out[..count], self.offset as u64)
            .map_err(std::io::Error::other)?;
        self.offset += count;
        Ok(count)
    }
}

pub trait GeneratedAssetStore: Send + Sync {
    /// A failure here happens after the provider effect already occurred, so callers must not retry
    /// automatically.
    fn register(
        &self,
        descriptor: OwnedFd,
        metadata: &NewAsset,
        capability: &str,
        invocation: &str,
    ) -> Result<u64, BlobError>;
    fn remove(&self, id: u64) -> Result<(), BlobError>;
    fn send(&self, id: u64) -> Result<Option<GeneratedImage>, BlobError>;
    fn delivery_failed(&self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetDeliveryDisposition {
    Abandoned,
    Failed,
    Delivered,
}

struct Queued {
    images: Vec<GeneratedImage>,
    ids: Vec<u64>,
    spent: u8,
    finished: bool,
}
pub struct ReplyAttachments {
    limit: u8,
    queued: Mutex<Queued>,
    registrar: Arc<dyn GeneratedAssetStore>,
    transport: String,
}
impl ReplyAttachments {
    pub fn new(limit: u8, registrar: Arc<dyn GeneratedAssetStore>, transport: String) -> Self {
        Self {
            limit,
            registrar,
            transport,
            queued: Mutex::new(Queued {
                images: Vec::new(),
                ids: Vec::new(),
                spent: 0,
                finished: false,
            }),
        }
    }
    pub fn remaining(&self) -> u8 {
        self.limit.saturating_sub(self.queued.lock().spent)
    }
    pub fn has_queued(&self) -> bool {
        !self.queued.lock().images.is_empty()
    }
    pub fn take(&self) -> Vec<GeneratedImage> {
        std::mem::take(&mut self.queued.lock().images)
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "reshaped by the unit that next rewrites this"
    )]
    pub fn receive(
        &self,
        attached: Vec<NewAsset>,
        descriptors: Vec<OwnedFd>,
        removed: Vec<u64>,
        sent: Vec<u64>,
        capability: &str,
        invocation: &str,
    ) -> String {
        let mut note = String::new();
        for id in removed {
            if let Err(error) = self.registrar.remove(id) {
                note.push_str(&format!("[gateway: asset removal refused: {error}]\n"));
            }
        }
        for id in sent {
            match self.registrar.send(id) {
                Ok(Some(image)) => {
                    let mut queued = self.queued.lock();
                    if queued.spent < self.limit {
                        queued.spent += 1;
                        queued.ids.push(id);
                        queued.images.push(image);
                    } else {
                        tracing::warn!(target: "dekopon_agent::audit", { audit.event = "agent.asset.send", asset.id = id, transport = self.transport, dispatched = false, error = "turn send allowance exhausted" }, "agent.asset.send");
                        note.push_str(
                            "[gateway: turn send allowance exhausted; asset was not queued]\n",
                        );
                        self.registrar.delivery_failed();
                    }
                }
                Ok(None) => (),
                Err(error) => {
                    tracing::warn!(target: "dekopon_agent::audit", { audit.event = "agent.asset.send", asset.id = id, transport = self.transport, dispatched = false, error = %error }, "agent.asset.send");
                    note.push_str(&format!(
                        "[gateway: chat-asset:{id} was not queued: {error}]\n"
                    ));
                    let mut queued = self.queued.lock();
                    // This accepted send spent broker allowance even though no upload can start.
                    queued.spent = queued.spent.saturating_add(1);
                    self.registrar.delivery_failed();
                }
            }
        }
        for (metadata, descriptor) in attached.into_iter().zip(descriptors) {
            match self.registrar.register(descriptor, &metadata, capability, invocation) {
                Ok(id) => {
                    let label: String = metadata.content_type.chars().filter(|c| !c.is_control()).take(128).collect();
                    note.push_str(&format!("[gateway: chat-asset:{id} ({label}, {} stored bytes) attached, not sent]\n", metadata.bytes));
                }
                Err(error) => note.push_str(&format!("[gateway: capability executed but its asset was not retained: {error}; do not repeat the paid call]\n")),
            }
        }
        note
    }
    pub fn finish(&self, disposition: AssetDeliveryDisposition) {
        let mut queued = self.queued.lock();
        if queued.finished {
            return;
        }
        queued.finished = true;
        let error = match disposition {
            AssetDeliveryDisposition::Abandoned => Some("turn ended before reply"),
            AssetDeliveryDisposition::Failed => Some("delivery failed"),
            AssetDeliveryDisposition::Delivered => None,
        };
        for id in &queued.ids {
            tracing::info!(target: "dekopon_agent::audit", { audit.event = "agent.asset.send", asset.id = id, transport = self.transport, dispatched = !matches!(disposition, AssetDeliveryDisposition::Abandoned), error }, "agent.asset.send");
        }
        if disposition != AssetDeliveryDisposition::Delivered && !queued.ids.is_empty() {
            self.registrar.delivery_failed();
        }
    }
}
impl Drop for ReplyAttachments {
    fn drop(&mut self) {
        self.finish(AssetDeliveryDisposition::Abandoned);
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ChatAssetRefusal {
    #[error("{0}")]
    Storage(BlobError),
    #[error("evicted: the asset was released; ask the user to resend it or select another asset")]
    Evicted,
    #[error("platform-gone: the platform no longer allows this attachment to be downloaded")]
    PlatformGone,
    #[error("store-failed: the attachment could not be retained; ask for another attachment")]
    StoreFailed,
    #[error("revoked: the asset is unavailable in this conversation generation")]
    Revoked,
    #[error("the conversation has no attachment with that number")]
    UnknownAsset,
    #[error("the attachment is not readable by this consumer")]
    UnsupportedMedia,
    #[error("one invocation may reference at most five distinct assets")]
    PerInvocationLimit,
    #[error("the assets exceed the 40 MiB per-invocation decoded-byte budget")]
    ByteBudget,
    #[error("the attachment's bytes could not be read")]
    Unavailable,
    #[error("data URLs are refused; use chat-asset:<N> instead")]
    DataUrl,
}
impl ChatAssetRefusal {
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Storage(_) => "storage",
            Self::Evicted => "evicted",
            Self::PlatformGone => "platform-gone",
            Self::StoreFailed => "store-failed",
            Self::Revoked => "revoked",
            Self::UnknownAsset => "unknown-asset",
            Self::UnsupportedMedia => "unsupported-media",
            Self::PerInvocationLimit => "per-invocation-limit",
            Self::ByteBudget => "byte-budget",
            Self::Unavailable => "unavailable",
            Self::DataUrl => "data-url",
        }
    }
    pub fn note(&self) -> String {
        self.to_string()
    }
}
pub struct ResolvedInput {
    pub row: AssetRow,
    pub blob: DiskBlob,
}
pub type ResolveFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResolvedInput, ChatAssetRefusal>> + Send + 'a>>;
pub trait ChatAssetSource: Send + Sync {
    /// Resolving a released (reclaimed) asset refuses permanently rather than trying to redownload
    /// it, since the underlying bytes are gone.
    fn resolve_input(&self, id: u64) -> ResolveFuture<'_>;
}
pub struct PreparedRun {
    pub argv: Vec<dekopon_broker_protocol::Arg>,
    pub assets: InvokeAssets,
    pub pins: Vec<DiskBlob>,
}

pub struct ChatAssetInputs {
    source: Arc<dyn ChatAssetSource>,
}
impl ChatAssetInputs {
    pub fn new(source: Arc<dyn ChatAssetSource>) -> Self {
        Self { source }
    }

    pub async fn prepare_run(
        &self,
        argv: &[dekopon_shell::Arg],
        sends_remaining: u8,
    ) -> Result<PreparedRun, ChatAssetRefusal> {
        let (argv, ids) = run_arguments(argv)?;
        let mut assets = InvokeAssets {
            sends_remaining,
            ..Default::default()
        };
        let mut pins = Vec::with_capacity(ids.len());
        let mut total = 0;
        for id in ids {
            let ResolvedInput { row, blob } = self.source.resolve_input(id).await?;
            let decoded = GeneratedImage::new(blob.clone(), row.content_type.clone(), row.encoding)
                .decoded_len()
                .map_err(ChatAssetRefusal::Storage)?;
            total = input_total(total, decoded)?;
            assets
                .descriptors
                .push(blob.descriptor().map_err(ChatAssetRefusal::Storage)?);
            assets.rows.push(row);
            pins.push(blob);
        }
        Ok(PreparedRun { argv, assets, pins })
    }
}
fn input_total(total: usize, bytes: usize) -> Result<usize, ChatAssetRefusal> {
    total
        .checked_add(bytes)
        .filter(|total| *total <= MAX_INVOCATION_ASSET_BYTES)
        .ok_or(ChatAssetRefusal::ByteBudget)
}

pub fn run_arguments(
    argv: &[dekopon_shell::Arg],
) -> Result<(Vec<dekopon_broker_protocol::Arg>, Vec<u64>), ChatAssetRefusal> {
    use dekopon_broker_protocol::{Arg, AssetIndex};
    let mut ids = Vec::new();
    let mut arguments = Vec::with_capacity(argv.len());
    for argument in argv {
        arguments.push(match argument {
            dekopon_shell::Arg::Text(text) => {
                let candidate = text
                    .split_once('=')
                    .map_or(text.as_str(), |(_, value)| value);
                if [text.as_str(), candidate].iter().any(|text| {
                    text.trim_start()
                        .get(..5)
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
                }) {
                    return Err(ChatAssetRefusal::DataUrl);
                }
                Arg::Text(text.clone())
            }
            dekopon_shell::Arg::Asset(number) => {
                let id = number.get();
                let index = match ids.iter().position(|known| *known == id) {
                    Some(index) => index,
                    None => {
                        if ids.len() == MAX_ASSET_DESCRIPTORS {
                            return Err(ChatAssetRefusal::PerInvocationLimit);
                        }
                        ids.push(id);
                        ids.len() - 1
                    }
                };
                Arg::Asset(
                    AssetIndex::try_from(
                        u8::try_from(index)
                            .map_err(|_overflow| ChatAssetRefusal::PerInvocationLimit)?,
                    )
                    .map_err(|_overflow| ChatAssetRefusal::PerInvocationLimit)?,
                )
            }
        });
    }
    Ok((arguments, ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_limit_matches_the_shared_contract() {
        assert_eq!(
            MAX_INVOCATION_ASSET_BYTES,
            dekopon_core::asset::MAX_DECODED_INVOCATION_BYTES
        );
    }

    use serde_json::json;
    use std::{fs::File, os::unix::fs::FileExt};

    struct Source {
        blobs: Vec<DiskBlob>,
    }
    fn row(id: u64, blob: &DiskBlob) -> AssetRow {
        AssetRow {
            id,
            content_type: "application/octet-stream".to_owned(),
            encoding: AssetEncoding::Identity,
            bytes: Some(blob.len() as u64),
            origin: "chat".to_owned(),
            sent: false,
        }
    }
    impl ChatAssetSource for Source {
        fn resolve_input(&self, id: u64) -> ResolveFuture<'_> {
            let resolved = self
                .blobs
                .get(id as usize - 1)
                .map(|blob| ResolvedInput {
                    row: row(id, blob),
                    blob: blob.clone(),
                })
                .ok_or(ChatAssetRefusal::UnknownAsset);
            Box::pin(async move { resolved })
        }
    }
    fn arguments(ids: &[u64]) -> Vec<dekopon_shell::Arg> {
        ids.iter()
            .map(|id| {
                dekopon_shell::Arg::Asset(
                    dekopon_core::AssetNumber::parse(&format!("chat-asset:{id}")).unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn only_named_rows_cross() {
        let inputs = ChatAssetInputs::new(Arc::new(Source {
            blobs: (1..=40)
                .map(|id| DiskBlob::from_bytes(&[id]).unwrap())
                .collect(),
        }));
        let PreparedRun { argv, assets, pins } =
            inputs.prepare_run(&arguments(&[7, 3, 7]), 4).await.unwrap();
        assert_eq!(
            assets.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [7, 3]
        );
        assert_eq!(pins.len(), 2);
        assert_eq!(
            serde_json::to_value(argv).unwrap(),
            json!([{"asset":0},{"asset":1},{"asset":0}])
        );
        for (descriptor, id) in assets.descriptors.into_iter().zip([7, 3]) {
            let file = File::from(descriptor);
            let mut byte = [0];
            file.read_exact_at(&mut byte, 0).unwrap();
            assert_eq!(byte, [id]);
            assert!(
                rustix::io::fcntl_getfd(&file)
                    .unwrap()
                    .contains(rustix::io::FdFlags::CLOEXEC)
            );
            assert_eq!(
                rustix::fs::fcntl_getfl(&file).unwrap() & rustix::fs::OFlags::ACCMODE,
                rustix::fs::OFlags::RDONLY
            );
        }
    }

    #[test]
    fn a_sixth_distinct_reference_is_refused_before_the_frame() {
        assert_eq!(
            run_arguments(&arguments(&[1, 2, 3, 4, 5, 6])),
            Err(ChatAssetRefusal::PerInvocationLimit)
        );
    }

    #[test]
    fn a_data_url_after_an_equals_sign_is_refused() {
        for text in ["data:text/plain,YQ==", "--image= DATA:text/plain,YQ=="] {
            assert_eq!(
                run_arguments(&[dekopon_shell::Arg::Text(text.to_owned())]),
                Err(ChatAssetRefusal::DataUrl)
            );
        }
        assert!(
            run_arguments(&[dekopon_shell::Arg::Text("xdata:text/plain,YQ==".to_owned())]).is_ok()
        );
    }

    #[tokio::test]
    async fn a_quoted_reference_does_not_open() {
        let inputs = ChatAssetInputs::new(Arc::new(Source { blobs: Vec::new() }));
        let PreparedRun { assets, pins, .. } = inputs
            .prepare_run(&[dekopon_shell::Arg::Text("chat-asset:9".to_owned())], 4)
            .await
            .unwrap();
        assert!(assets.rows.is_empty());
        assert!(assets.descriptors.is_empty());
        assert!(pins.is_empty());
        assert!(matches!(
            inputs.prepare_run(&arguments(&[9]), 4).await,
            Err(ChatAssetRefusal::UnknownAsset)
        ));
    }

    #[tokio::test]
    async fn five_eight_mib_inputs_fit() {
        let inputs = ChatAssetInputs::new(Arc::new(Source {
            blobs: (0..5)
                .map(|_| DiskBlob::from_bytes(&vec![0; MAX_ATTACHMENT_BYTES]).unwrap())
                .collect(),
        }));
        let PreparedRun { assets, pins, .. } = inputs
            .prepare_run(&arguments(&[1, 2, 3, 4, 5]), 4)
            .await
            .unwrap();
        assert_eq!(assets.descriptors.len(), 5);
        assert_eq!(
            pins.iter().map(DiskBlob::len).sum::<usize>(),
            MAX_INVOCATION_ASSET_BYTES
        );
    }

    #[test]
    fn invocation_decoded_byte_budget_accepts_the_edge_and_refuses_one_past() {
        assert_eq!(
            input_total(MAX_INVOCATION_ASSET_BYTES - 1, 1),
            Ok(MAX_INVOCATION_ASSET_BYTES)
        );
        assert_eq!(
            input_total(MAX_INVOCATION_ASSET_BYTES, 1),
            Err(ChatAssetRefusal::ByteBudget)
        );
        assert_eq!(
            input_total(usize::MAX, 1),
            Err(ChatAssetRefusal::ByteBudget)
        );
    }

    #[test]
    fn an_authorized_mp4_send_keeps_its_name_and_bytes() {
        use dekopon_test_support::CaptureLayer;
        use tracing_subscriber::prelude::*;

        struct Store(std::sync::atomic::AtomicUsize);
        impl GeneratedAssetStore for Store {
            fn register(
                &self,
                descriptor: OwnedFd,
                metadata: &NewAsset,
                _: &str,
                _: &str,
            ) -> Result<u64, BlobError> {
                assert_eq!(metadata.content_type, "video/mp4");
                let file = File::from(descriptor);
                let mut bytes = [0; 10];
                file.read_exact_at(&mut bytes, 0).unwrap();
                assert_eq!(&bytes, b"mp4\0frames");
                Ok(7)
            }
            fn remove(&self, _: u64) -> Result<(), BlobError> {
                unreachable!()
            }
            fn send(&self, id: u64) -> Result<Option<GeneratedImage>, BlobError> {
                assert_eq!(id, 7);
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Some(GeneratedImage::new(
                    DiskBlob::from_bytes(b"mp4\0frames")?,
                    "video/mp4".into(),
                    AssetEncoding::Identity,
                )))
            }
            fn delivery_failed(&self) {}
        }
        let capture = CaptureLayer::workspace();
        let _guard = tracing_subscriber::registry()
            .with(capture.clone())
            .set_default();
        {
            let store = Arc::new(Store(std::sync::atomic::AtomicUsize::new(0)));
            let queue = ReplyAttachments::new(1, Arc::<Store>::clone(&store), "local".into());
            let blob = DiskBlob::from_bytes(b"mp4\0frames").unwrap();
            let metadata = NewAsset {
                descriptor: 0,
                content_type: "video/mp4".into(),
                encoding: AssetEncoding::Identity,
                bytes: 10,
                sha256: String::new(),
            };
            assert!(
                queue
                    .receive(
                        vec![metadata],
                        vec![blob.descriptor().unwrap()],
                        vec![],
                        vec![],
                        "",
                        ""
                    )
                    .contains("attached, not sent")
            );
            assert!(queue.take().is_empty(), "attachment alone is not a send");
            assert_eq!(store.0.load(std::sync::atomic::Ordering::SeqCst), 0);
            queue.receive(vec![], vec![], vec![], vec![7], "", "");
            let images = queue.take();
            assert_eq!(images[0].filename(0), "asset-1.mp4");
            assert_eq!(images[0].media_type(), "video/mp4");
            assert_eq!(images[0].bytes().unwrap(), b"mp4\0frames");
            queue.finish(AssetDeliveryDisposition::Delivered);
            let cancelled = ReplyAttachments::new(1, store, "local".into());
            cancelled.receive(vec![], vec![], vec![], vec![7], "", "");
            cancelled.finish(AssetDeliveryDisposition::Abandoned);
        }
        assert_eq!(
            capture
                .events_where(
                    "dekopon_agent::audit",
                    &[
                        ("audit.event", "agent.asset.send"),
                        ("transport", "local"),
                        ("dispatched", "true")
                    ]
                )
                .len(),
            1
        );
        assert_eq!(
            capture
                .events_where(
                    "dekopon_agent::audit",
                    &[("audit.event", "agent.asset.send"), ("dispatched", "false")]
                )
                .len(),
            1
        );
    }

    #[test]
    fn outbound_names_follow_the_content_type() {
        for (content_type, extension) in [
            ("text/markdown", "md"),
            ("text/x-unknown", "txt"),
            ("application/octet-stream", "bin"),
            ("text/markdown; charset=utf-8", "md"),
            ("Text/Markdown", "md"),
            (" text/markdown ; charset=utf-8; ignored=true", "md"),
            ("text/csv", "csv"),
            ("application/json", "json"),
            ("text/html", "html"),
            ("text/xml", "xml"),
            ("application/xml", "xml"),
            ("application/msword", "doc"),
            (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "docx",
            ),
            (
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "pptx",
            ),
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "xlsx",
            ),
            ("application/rtf", "rtf"),
        ] {
            let image = GeneratedImage::new(
                DiskBlob::from_bytes(b"attachment").unwrap(),
                content_type.to_owned(),
                AssetEncoding::Identity,
            );
            assert_eq!(
                image.filename(0),
                format!("asset-1.{extension}"),
                "{content_type}"
            );
        }
    }

    #[test]
    fn raw_and_base64_assets_have_the_same_delivery_bytes() {
        use base64::Engine as _;
        let raw = b"binary\0payload";
        for (bytes, encoding) in [
            (raw.to_vec(), AssetEncoding::Identity),
            (STANDARD.encode(raw).into_bytes(), AssetEncoding::Base64),
        ] {
            let image = GeneratedImage::new(
                DiskBlob::from_bytes(&bytes).unwrap(),
                "application/octet-stream".to_owned(),
                encoding,
            );
            assert_eq!(
                image.len(),
                bytes.len(),
                "retention still counts stored bytes"
            );
            assert_eq!(image.decoded_len().unwrap(), raw.len());
            assert_eq!(image.bytes().unwrap(), raw);
            assert!(!format!("{image:?}").contains("payload"));
        }
    }
    #[test]
    fn decoded_upload_lengths_handle_padding_empty_and_short_invalid_storage() {
        use base64::Engine as _;
        for raw in [b"".as_slice(), b"a", b"ab", b"abc"] {
            let image = GeneratedImage::new(
                DiskBlob::from_bytes(STANDARD.encode(raw).as_bytes()).unwrap(),
                "text/plain".to_owned(),
                AssetEncoding::Base64,
            );
            assert_eq!(image.decoded_len().unwrap(), raw.len());
            assert_eq!(image.bytes().unwrap(), raw);
        }
        for invalid in [b"x".as_slice(), b"xx", b"xxx"] {
            let image = GeneratedImage::new(
                DiskBlob::from_bytes(invalid).unwrap(),
                "text/plain".to_owned(),
                AssetEncoding::Base64,
            );
            assert_eq!(
                image.decoded_len(),
                Err(BlobError::Io(std::io::ErrorKind::InvalidData))
            );
        }
    }
}
