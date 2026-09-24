use dekopon_broker_protocol::{
    AssetEncoding, AssetRow, InvokeAssets, MAX_DESCRIPTORS_PER_FRAME, NewAsset,
};
use dekopon_core::{
    base64::{DecoderReader, STANDARD},
    chat_asset_marker,
};
use dekopon_model::asset::{BlobError, DiskBlob};
use serde_json::Value;
use std::{
    fmt,
    io::Read,
    os::fd::OwnedFd,
    sync::{Arc, Mutex},
};
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
        let extension = match self.content_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "text/plain" => "txt",
            "application/pdf" => "pdf",
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
        self.limit.saturating_sub(
            self.queued
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .spent,
        )
    }
    pub fn has_queued(&self) -> bool {
        !self
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .images
            .is_empty()
    }
    pub fn take(&self) -> Vec<GeneratedImage> {
        std::mem::take(
            &mut self
                .queued
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .images,
        )
    }
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
                    let mut queued = self
                        .queued
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if queued.spent < self.limit {
                        queued.spent += 1;
                        queued.ids.push(id);
                        queued.images.push(image);
                    } else {
                        tracing::warn!(target: "dekopon_agent::audit", { audit.event = "agent.asset.send", asset.id = id, transport = self.transport, dispatched = false, error = "turn send allowance exhausted" }, "asset send failed");
                        note.push_str(
                            "[gateway: turn send allowance exhausted; asset was not queued]\n",
                        );
                        self.registrar.delivery_failed();
                    }
                }
                Ok(None) => (),
                Err(error) => {
                    tracing::warn!(target: "dekopon_agent::audit", { audit.event = "agent.asset.send", asset.id = id, transport = self.transport, dispatched = false, error = %error }, "asset send failed");
                    note.push_str(&format!(
                        "[gateway: chat-asset:{id} was not queued: {error}]\n"
                    ));
                    let mut queued = self
                        .queued
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut queued = self
            .queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            tracing::info!(target: "dekopon_agent::audit", { audit.event = "agent.asset.send", asset.id = id, transport = self.transport, dispatched = !matches!(disposition, AssetDeliveryDisposition::Abandoned), error }, "asset delivery disposition");
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
    #[error("the asset was released; ask the user to resend it or select another asset")]
    Reclaimed,
    #[error("the asset is unavailable in this conversation generation")]
    Unauthorized,
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
            Self::Reclaimed => "reclaimed",
            Self::Unauthorized => "unauthorized",
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
pub trait ChatAssetSource: Send + Sync {
    /// Fetching a released (reclaimed) asset refuses permanently rather than trying to redownload
    /// it, since the underlying bytes are gone.
    fn fetch_for_capability(&self, id: u64) -> Result<(String, DiskBlob), ChatAssetRefusal>;
    fn rows(&self) -> Vec<AssetRow>;
}
pub struct ChatAssetInputs {
    source: Arc<dyn ChatAssetSource>,
}
impl ChatAssetInputs {
    pub fn new(source: Arc<dyn ChatAssetSource>) -> Self {
        Self { source }
    }
    pub fn prepare(
        &self,
        input: &Value,
        sends_remaining: u8,
    ) -> Result<(InvokeAssets, Vec<DiskBlob>), ChatAssetRefusal> {
        let references = references(input)?;
        let mut pins = Vec::with_capacity(references.len());
        let mut descriptors = Vec::with_capacity(references.len());
        for id in &references {
            let (_, blob) = self.source.fetch_for_capability(*id)?;
            descriptors.push(blob.descriptor().map_err(ChatAssetRefusal::Storage)?);
            pins.push(blob);
        }
        // Fetch may populate the inventory's stored lengths; take the table after pinning.
        let rows = self.source.rows();
        let mut total = 0usize;
        for (id, blob) in references.iter().zip(&pins) {
            let row = rows
                .iter()
                .find(|row| row.id == *id)
                .ok_or(ChatAssetRefusal::UnknownAsset)?;
            let decoded = GeneratedImage::new(blob.clone(), row.content_type.clone(), row.encoding)
                .decoded_len()
                .map_err(ChatAssetRefusal::Storage)?;
            total = input_total(total, decoded)?;
        }
        Ok((
            InvokeAssets {
                rows,
                descriptors,
                sends_remaining,
            },
            pins,
        ))
    }
}
fn input_total(total: usize, bytes: usize) -> Result<usize, ChatAssetRefusal> {
    total
        .checked_add(bytes)
        .filter(|total| *total <= MAX_INVOCATION_ASSET_BYTES)
        .ok_or(ChatAssetRefusal::ByteBudget)
}

pub fn references(input: &Value) -> Result<Vec<u64>, ChatAssetRefusal> {
    fn walk(input: &Value, ids: &mut Vec<u64>) -> Result<(), ChatAssetRefusal> {
        match input {
            Value::String(text) => {
                if text
                    .trim_start()
                    .get(..5)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
                {
                    return Err(ChatAssetRefusal::DataUrl);
                }
                if let Some(id) = chat_asset_marker(text)
                    && !ids.contains(&id)
                {
                    if ids.len() == MAX_DESCRIPTORS_PER_FRAME {
                        return Err(ChatAssetRefusal::PerInvocationLimit);
                    }
                    ids.push(id);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, ids)?;
                }
            }
            Value::Object(fields) => {
                for value in fields.values() {
                    walk(value, ids)?;
                }
            }
            _ => (),
        }
        Ok(())
    }
    let mut ids = Vec::new();
    walk(input, &mut ids)?;
    Ok(ids)
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
    impl ChatAssetSource for Source {
        fn fetch_for_capability(&self, id: u64) -> Result<(String, DiskBlob), ChatAssetRefusal> {
            self.blobs
                .get(id as usize - 1)
                .cloned()
                .map(|blob| ("application/octet-stream".to_owned(), blob))
                .ok_or(ChatAssetRefusal::UnknownAsset)
        }
        fn rows(&self) -> Vec<AssetRow> {
            self.blobs
                .iter()
                .enumerate()
                .map(|(index, blob)| AssetRow {
                    id: index as u64 + 1,
                    content_type: "application/octet-stream".to_owned(),
                    encoding: AssetEncoding::Identity,
                    bytes: Some(blob.len() as u64),
                    origin: "chat".to_owned(),
                    sent: false,
                })
                .collect()
        }
    }
    #[test]
    fn references_are_distinct_in_first_occurrence_order_and_never_expanded() {
        let proposal = json!(["chat-asset:2", {"image":"chat-asset:1"}, "chat-asset:2"]);
        assert_eq!(references(&proposal), Ok(vec![2, 1]));
        let a = DiskBlob::from_bytes(b"first").unwrap();
        let b = DiskBlob::from_descriptor(
            DiskBlob::from_bytes(b"second")
                .unwrap()
                .descriptor()
                .unwrap(),
            6,
        )
        .unwrap();
        let inputs = ChatAssetInputs::new(Arc::new(Source { blobs: vec![a, b] }));
        for _ in 0..2 {
            let (assets, pins) = inputs.prepare(&proposal, 3).unwrap();
            assert_eq!(assets.rows.len(), 2);
            assert_eq!(assets.sends_remaining, 3);
            assert_eq!(pins.len(), 2);
            for (fd, expected) in assets
                .descriptors
                .into_iter()
                .zip([b"second".as_slice(), b"first".as_slice()])
            {
                let file = File::from(fd);
                assert!(
                    rustix::io::fcntl_getfd(&file)
                        .unwrap()
                        .contains(rustix::io::FdFlags::CLOEXEC)
                );
                assert_eq!(
                    rustix::fs::fcntl_getfl(&file).unwrap() & rustix::fs::OFlags::ACCMODE,
                    rustix::fs::OFlags::RDONLY
                );
                let mut bytes = vec![0; expected.len()];
                file.read_exact_at(&mut bytes, 0).unwrap();
                assert_eq!(bytes, expected);
            }
        }
        assert_eq!(proposal[0], "chat-asset:2");
    }
    #[test]
    fn five_eight_mib_inputs_fit_and_a_sixth_reference_is_refused_before_fetch() {
        let inputs = ChatAssetInputs::new(Arc::new(Source {
            blobs: (0..5)
                .map(|_| DiskBlob::from_bytes(&vec![0; MAX_ATTACHMENT_BYTES]).unwrap())
                .collect(),
        }));
        let proposal = json!([
            "chat-asset:1",
            "chat-asset:2",
            "chat-asset:3",
            "chat-asset:4",
            "chat-asset:5"
        ]);
        let (assets, pins) = inputs.prepare(&proposal, 4).unwrap();
        assert_eq!(assets.descriptors.len(), 5);
        assert_eq!(
            pins.iter().map(DiskBlob::len).sum::<usize>(),
            MAX_INVOCATION_ASSET_BYTES
        );
        assert_eq!(
            references(&json!([
                "chat-asset:1",
                "chat-asset:2",
                "chat-asset:3",
                "chat-asset:4",
                "chat-asset:5",
                "chat-asset:6"
            ])),
            Err(ChatAssetRefusal::PerInvocationLimit)
        );
    }
    #[test]
    fn proposal_data_urls_are_refused_in_every_nested_string_position() {
        for input in [
            json!("data:image/png;base64,UE5H"),
            json!({"nested":["DATA:text/plain;base64,YQ=="]}),
            json!([" data:image/jpeg;base64,YQ=="]),
        ] {
            assert_eq!(references(&input), Err(ChatAssetRefusal::DataUrl));
        }
        assert!(ChatAssetRefusal::DataUrl.note().contains("chat-asset:<N>"));
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
