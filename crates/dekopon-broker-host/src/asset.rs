//! Per-invocation asset resources. Descriptors carry bytes; table rows carry only metadata.

use crate::{StoreState, bindings::dekopon::asset::asset as wit};
use dekopon_broker_protocol::{
    AssetEncoding, AssetRow, MAX_ASSET_ROWS, MAX_DESCRIPTORS_PER_FRAME, NewAsset,
};
use dekopon_capability::AssetConstraints;
use dekopon_core::{
    base64::{self, Validator},
    chat_asset_marker as reference,
};
use dekopon_http_host::{
    CHUNK_BYTES, FilePart, Representation,
    asset::{AssetDirectory, AssetFile, AssetIoError, AssetJobs, AssetReader, Spool},
    read_decoded,
};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    fs::File,
    os::{fd::OwnedFd, unix::fs::FileExt as _},
    sync::Arc,
};
use wasmtime::component::Resource;

const MAX_ASSET_BYTES: u64 = dekopon_core::asset::MAX_DECODED_ASSET_BYTES as u64;
const MAX_INVOCATION_BYTES: u64 = dekopon_core::asset::MAX_DECODED_INVOCATION_BYTES as u64;

/// A frame's descriptors or table do not satisfy the asset admission contract.
#[derive(Debug, thiserror::Error)]
pub enum AssetAdmissionError {
    /// Table row ceiling exceeded.
    #[error("too many asset rows")]
    TooManyRows,
    /// Reference order must zip exactly with at most five descriptors.
    #[error("asset descriptor count does not match references")]
    DescriptorCount,
    /// A conversation number must name exactly one table row.
    #[error("duplicate asset table row")]
    DuplicateRow,
    /// A referenced number is absent from the conversation table.
    #[error("asset reference absent from table")]
    UnknownReference,
    /// Only matching regular, read-only files are admitted.
    #[error("asset descriptor is not a matching read-only file")]
    InvalidDescriptor,
    /// Input file lengths exceed the invocation ceiling.
    #[error("asset input exceeds invocation byte limit")]
    TooLarge,
    /// Native descriptor inspection failed.
    #[error("asset descriptor inspection failed")]
    Io(#[from] AssetIoError),
}

/// Gateway metadata and the descriptors associated with proposal references in discovery order.
#[derive(Debug, Default)]
pub struct AssetInputs {
    /// Complete conversation table, at most 32 rows.
    pub rows: Vec<AssetRow>,
    /// One read-only descriptor per distinct referenced string leaf.
    pub descriptors: Vec<OwnedFd>,
    /// Remaining external-delivery allowance for this turn.
    pub sends_remaining: u8,
}

/// Successful invocation's typed changes; keeping files alive retains in-flight accounting.
#[derive(Debug, Default)]
pub struct AssetOutputs {
    /// Metadata in descriptor order.
    pub attached: Vec<NewAsset>,
    /// Conversation references removed by this invocation.
    pub removed: Vec<u64>,
    /// Conversation references marked for delivery.
    pub sent: Vec<u64>,
    /// Read-only files corresponding exactly to attached rows.
    pub files: Vec<Arc<AssetFile>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AssetId(u64);

/// File-backed host handle; never exposes its descriptor to a guest.
pub struct HandleResource {
    digest_recorded: bool,
    source: Source,
    info: wit::Info,
}

enum Source {
    File {
        fd: AssetReader,
        cursor: u64,
        len: u64,
    },
}

/// Sequential host writer under the invocation and process byte ceilings.
pub struct WriterResource {
    spool: Option<WriterSink>,
    content_type: String,
    encoding: wit::Encoding,
    validator: Validator,
}

enum WriterSink {
    File(Spool),
    #[cfg(test)]
    Channel {
        sender: tokio::sync::mpsc::Sender<Vec<u8>>,
        bytes: u64,
    },
}

impl WriterSink {
    fn len(&self) -> u64 {
        match self {
            Self::File(spool) => spool.len(),
            #[cfg(test)]
            Self::Channel { bytes, .. } => *bytes,
        }
    }
    async fn write(self, chunk: Vec<u8>) -> Result<Self, AssetIoError> {
        match self {
            Self::File(spool) => spool.write(chunk).await.map(Self::File),
            #[cfg(test)]
            Self::Channel { sender, bytes } => {
                let bytes = bytes + chunk.len() as u64;
                sender
                    .send(chunk)
                    .await
                    .map_err(|_closed| AssetIoError::Io {
                        kind: std::io::ErrorKind::BrokenPipe,
                    })?;
                Ok(Self::Channel { sender, bytes })
            }
        }
    }
    async fn finish(self) -> Result<AssetFile, AssetIoError> {
        match self {
            Self::File(spool) => spool.finish().await,
            #[cfg(test)]
            Self::Channel { .. } => Err(AssetIoError::Io {
                kind: std::io::ErrorKind::Unsupported,
            }),
        }
    }
}

pub(crate) struct AssetState {
    jobs: AssetJobs,
    #[cfg(test)]
    channel_sink: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    origin: String,
    violation: Option<wit::ErrorCode>,
    active: bool,
    attempted: bool,
    directory: Option<AssetDirectory>,
    grant: AssetConstraints,
    rows: Vec<AssetRow>,
    inputs: Vec<(AssetId, AssetReader)>,
    sends_remaining: u8,
    /// Decoded writer bytes and every streamed asset occurrence share the invocation budget.
    written: u64,
    outputs: AssetOutputs,
}

impl AssetState {
    pub(crate) fn disabled() -> Self {
        Self {
            #[cfg(test)]
            channel_sink: None,
            jobs: AssetJobs::default(),
            origin: String::new(),
            violation: None,
            active: false,
            attempted: false,
            directory: None,
            grant: AssetConstraints::default(),
            rows: vec![],
            inputs: vec![],
            sends_remaining: 0,
            written: 0,
            outputs: AssetOutputs::default(),
        }
    }

    pub(crate) fn reject(&mut self, code: wit::ErrorCode) {
        if self.violation.is_none() || matches!(code, wit::ErrorCode::OverBudget) {
            self.violation = Some(code);
        }
    }

    pub(crate) fn violation(&self) -> Option<wit::ErrorCode> {
        self.violation
    }

    pub(crate) fn refuse<T>(&mut self, failure: wit::Error) -> Result<T, wit::Error> {
        self.reject(failure.code);
        Err(failure)
    }

    pub(crate) fn charge_stream(&mut self, decoded_bytes: u64) -> Result<(), wit::Error> {
        let Some(total) = self
            .written
            .checked_add(decoded_bytes)
            .filter(|bytes| *bytes <= MAX_INVOCATION_BYTES)
        else {
            return self.refuse(error(
                wit::ErrorCode::TooLarge,
                "streamed asset parts exceed the decoded invocation byte ceiling",
            ));
        };
        self.written = total;
        Ok(())
    }

    pub(crate) fn attempted(&self) -> bool {
        self.attempted
    }

    pub(crate) async fn invoke(
        inputs: AssetInputs,
        references: Vec<u64>,
        grant: AssetConstraints,
        directory: Option<AssetDirectory>,
        origin: String,
    ) -> Result<Self, AssetAdmissionError> {
        let jobs = AssetJobs::default();
        tokio::task::spawn_blocking(move || {
            if inputs.rows.len() > MAX_ASSET_ROWS {
                return Err(AssetAdmissionError::TooManyRows);
            }
            if references.len() != inputs.descriptors.len()
                || references.len() > MAX_DESCRIPTORS_PER_FRAME
            {
                return Err(AssetAdmissionError::DescriptorCount);
            }
            let ids: BTreeSet<_> = inputs.rows.iter().map(|row| row.id).collect();
            if ids.len() != inputs.rows.len() {
                return Err(AssetAdmissionError::DuplicateRow);
            }
            let mut passed = Vec::with_capacity(references.len());
            let mut total = 0_u64;
            for (id, fd) in references.into_iter().zip(inputs.descriptors) {
                let row = inputs
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .ok_or(AssetAdmissionError::UnknownReference)?;
                let file = File::from(fd);
                let metadata = file.metadata().map_err(AssetIoError::from)?;
                let flags = rustix::fs::fcntl_getfl(&file)
                    .map_err(|error| AssetIoError::from(std::io::Error::from(error)))?;
                if !metadata.is_file()
                    || flags & rustix::fs::OFlags::ACCMODE != rustix::fs::OFlags::RDONLY
                    || metadata.len() != row.bytes
                {
                    return Err(AssetAdmissionError::InvalidDescriptor);
                }
                let decoded = file_decoded_length(&file, encoding(row.encoding), metadata.len())?;
                total = total
                    .checked_add(decoded)
                    .ok_or(AssetAdmissionError::TooLarge)?;
                if total > MAX_INVOCATION_BYTES {
                    return Err(AssetAdmissionError::TooLarge);
                }
                passed.push((AssetId(id), AssetReader::input(file, jobs.clone())));
            }
            Ok(Self {
                #[cfg(test)]
                channel_sink: None,
                origin,
                violation: None,
                active: true,
                attempted: false,
                directory: directory.map(|directory| directory.invocation(jobs.clone())),
                jobs,
                grant,
                rows: inputs.rows,
                inputs: passed,
                sends_remaining: inputs.sends_remaining,
                written: 0,
                outputs: AssetOutputs::default(),
            })
        })
        .await
        .map_err(|_join| AssetAdmissionError::Io(AssetIoError::Worker))?
    }

    fn require_active(&mut self) -> wasmtime::Result<()> {
        self.attempted = true;
        if !self.active {
            wasmtime::bail!("asset imports are available only during invoke");
        }
        Ok(())
    }

    pub(crate) async fn drain(&self) {
        self.jobs.drain().await;
    }

    pub(crate) fn finish(&mut self) -> AssetOutputs {
        std::mem::take(&mut self.outputs)
    }

    pub(crate) fn directory(&mut self) -> wasmtime::Result<Option<AssetDirectory>> {
        self.require_active()?;
        Ok(self.directory.clone())
    }
}

pub(crate) fn references(input: &serde_json::Value) -> Vec<u64> {
    fn walk(input: &serde_json::Value, found: &mut Vec<u64>) {
        match input {
            serde_json::Value::String(text) => {
                if let Some(id) = reference(text)
                    && found.len() <= MAX_DESCRIPTORS_PER_FRAME
                    && !found.contains(&id)
                {
                    found.push(id);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    walk(value, found);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values() {
                    walk(value, found);
                }
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    walk(input, &mut found);
    found
}

fn error(code: wit::ErrorCode, message: &str) -> wit::Error {
    wit::Error {
        code,
        message: message.to_owned(),
    }
}
fn io_error(error_value: AssetIoError) -> wit::Error {
    match error_value {
        AssetIoError::OverBudget => error(
            wit::ErrorCode::OverBudget,
            "broker asset byte budget is exhausted",
        ),
        AssetIoError::Io {
            kind: std::io::ErrorKind::InvalidData,
        } => error(wit::ErrorCode::InvalidEncoding, "invalid base64 asset"),
        AssetIoError::Io { kind } => {
            tracing::warn!(?kind, "asset I/O failed");
            error(wit::ErrorCode::Io, "asset I/O failed")
        }
        AssetIoError::Worker => error(wit::ErrorCode::Internal, "asset worker failed"),
    }
}
fn encoding(value: AssetEncoding) -> wit::Encoding {
    match value {
        AssetEncoding::Identity => wit::Encoding::Identity,
        AssetEncoding::Base64 => wit::Encoding::Base64,
    }
}
fn wire_encoding(value: wit::Encoding) -> AssetEncoding {
    match value {
        wit::Encoding::Identity => AssetEncoding::Identity,
        wit::Encoding::Base64 => AssetEncoding::Base64,
    }
}
fn representation(value: wit::Encoding) -> Representation {
    match value {
        wit::Encoding::Identity => Representation::Identity,
        wit::Encoding::Base64 => Representation::Base64,
    }
}
fn row_info(row: &AssetRow) -> wit::Info {
    wit::Info {
        id: Some(row.id),
        content_type: row.content_type.clone(),
        encoding: encoding(row.encoding),
        stored_bytes: Some(row.bytes),
        seekable: true,
        origin: row.origin.clone(),
        sent: row.sent,
    }
}

async fn decoded_length(
    file: AssetReader,
    value: wit::Encoding,
    stored: u64,
) -> Result<u64, wit::Error> {
    if matches!(value, wit::Encoding::Identity) {
        return Ok(stored);
    }
    file.read(move |file| file_decoded_length(file, value, stored))
        .await
        .map_err(io_error)
}

fn file_decoded_length(
    file: &File,
    value: wit::Encoding,
    stored: u64,
) -> Result<u64, AssetIoError> {
    if matches!(value, wit::Encoding::Identity) || stored == 0 {
        return Ok(stored);
    }
    let invalid = || AssetIoError::Io {
        kind: std::io::ErrorKind::InvalidData,
    };
    let mut tail = [0; 2];
    let offset = stored.checked_sub(2).ok_or_else(invalid)?;
    file.read_exact_at(&mut tail, offset)?;
    base64::decoded_len(stored, &tail).map_err(|_invalid| invalid())
}

impl HandleResource {
    async fn digest(&self) -> Result<String, wit::Error> {
        let Source::File { fd, len, .. } = &self.source;
        let file = fd.clone();
        let len = *len;
        let representation = representation(self.info.encoding);
        let span = tracing::Span::current();
        file.read(move |file| {
            span.in_scope(|| {
                let mut hash = Sha256::new();
                let mut cursor = 0;
                while cursor < len {
                    let bytes = read_decoded(
                        file,
                        representation,
                        cursor,
                        (len - cursor).min(CHUNK_BYTES as u64) as usize,
                    )?;
                    cursor += bytes.len() as u64;
                    hash.update(&bytes);
                }
                Ok::<_, AssetIoError>(
                    hash.finalize()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                )
            })
        })
        .await
        .map_err(io_error)
    }

    async fn read_at(&mut self, offset: u64, count: u32) -> Result<Vec<u8>, wit::Error> {
        if self.info.id.is_some() && !self.digest_recorded {
            let digest = self.digest().await?;
            tracing::info!(
                "asset.id" = self.info.id,
                "asset.content_type" = self.info.content_type,
                "asset.bytes" = self.info.stored_bytes,
                "asset.sha256" = digest,
                "opened asset consumed through WIT"
            );
            self.digest_recorded = true;
        }
        let Source::File { fd, len, .. } = &self.source;
        let count = len
            .saturating_sub(offset)
            .min(u64::from(count))
            .min(CHUNK_BYTES as u64) as usize;
        if count == 0 {
            return Ok(Vec::new());
        }
        let file = fd.clone();
        let representation = representation(self.info.encoding);
        file.read(move |file| {
            read_decoded(file, representation, offset, count).map_err(AssetIoError::from)
        })
        .await
        .map_err(io_error)
    }

    pub(crate) fn http_part(&self, wire: wit::Encoding) -> FilePart {
        let Source::File { fd, len, .. } = &self.source;
        FilePart {
            file: fd.clone(),
            stored: representation(self.info.encoding),
            wire: representation(wire),
            decoded_bytes: *len,
            id: self.info.id,
            content_type: self.info.content_type.clone(),
        }
    }

    pub(crate) async fn from_output(
        file: AssetFile,
        content_type: String,
        value: wit::Encoding,
        origin: String,
    ) -> Result<Self, wit::Error> {
        let stored = file.len();
        let fd = AssetReader::output(Arc::new(file));
        let len = decoded_length(fd.clone(), value, stored).await?;
        Ok(Self {
            digest_recorded: false,
            source: Source::File { fd, cursor: 0, len },
            info: wit::Info {
                id: None,
                content_type,
                encoding: value,
                stored_bytes: Some(stored),
                seekable: true,
                origin,
                sent: false,
            },
        })
    }
}

impl wit::HostHandle for StoreState {
    async fn info(&mut self, resource: Resource<HandleResource>) -> wasmtime::Result<wit::Info> {
        self.assets.require_active()?;
        let handle = self.table.get(&resource)?;
        let mut info = handle.info.clone();
        if let Some(id) = info.id {
            info.sent |= self.assets.outputs.sent.contains(&id);
        }
        Ok(info)
    }
    async fn read(
        &mut self,
        resource: Resource<HandleResource>,
        len: u32,
    ) -> wasmtime::Result<Result<Vec<u8>, wit::Error>> {
        self.assets.require_active()?;
        let handle = self.table.get_mut(&resource)?;
        let Source::File { cursor, .. } = &handle.source;
        let cursor = *cursor;
        let result = handle.read_at(cursor, len).await;
        if let Ok(bytes) = &result {
            let Source::File { cursor, .. } = &mut handle.source;
            *cursor += bytes.len() as u64;
        }
        if let Err(failure) = &result {
            self.assets.violation.get_or_insert(failure.code);
        }
        Ok(result)
    }
    async fn read_at(
        &mut self,
        resource: Resource<HandleResource>,
        offset: u64,
        len: u32,
    ) -> wasmtime::Result<Result<Vec<u8>, wit::Error>> {
        self.assets.require_active()?;
        let result = self.table.get_mut(&resource)?.read_at(offset, len).await;
        if let Err(failure) = &result {
            self.assets.violation.get_or_insert(failure.code);
        }
        Ok(result)
    }
    async fn drop(&mut self, resource: Resource<HandleResource>) -> wasmtime::Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

// bindgen eagerly lifts anonymous lists into Vec. Register this one import with WasmList so
// the length is checked in guest memory before any payload allocation; the WIT remains async.
pub(crate) fn link_bounded_writer(
    linker: &mut wasmtime::component::Linker<StoreState>,
) -> wasmtime::Result<()> {
    use wasmtime::component::WasmList;
    linker.allow_shadowing(true);
    linker
        .instance("dekopon:asset/asset@0.1.0")?
        .func_wrap_async(
            "[method]writer.write",
            |mut store, (resource, bytes): (Resource<WriterResource>, WasmList<u8>)| {
                Box::new(async move {
                    if bytes.len() > CHUNK_BYTES {
                        tracing::info!(
                            "asset.write.guest_bytes" = bytes.len(),
                            "asset.write.copied_bytes" = 0,
                            "rejected oversized direct WIT list before copying"
                        );
                        let state = store.data_mut();
                        state.assets.require_active()?;
                        let result: Result<(), wit::Error> = state.assets.refuse(error(
                            wit::ErrorCode::TooLarge,
                            "asset write exceeds its byte ceiling",
                        ));
                        return Ok((result,));
                    }
                    let bytes = bytes.as_le_slice(&store).to_vec();
                    tracing::info!(
                        "asset.write.guest_bytes" = bytes.len(),
                        "asset.write.copied_bytes" = bytes.len(),
                        "bounded direct WIT list copy"
                    );
                    Ok((wit::HostWriter::write(store.data_mut(), resource, bytes).await?,))
                })
            },
        )?;
    linker.allow_shadowing(false);
    Ok(())
}

impl wit::HostWriter for StoreState {
    async fn write(
        &mut self,
        resource: Resource<WriterResource>,
        bytes: Vec<u8>,
    ) -> wasmtime::Result<Result<(), wit::Error>> {
        self.assets.require_active()?;
        let writer = self.table.get_mut(&resource)?;
        let Some(spool) = writer.spool.take() else {
            return Ok(self
                .assets
                .refuse(error(wit::ErrorCode::Io, "asset writer is closed")));
        };
        let next = spool.len().checked_add(bytes.len() as u64);
        let stored_limit = match writer.encoding {
            wit::Encoding::Identity => MAX_ASSET_BYTES,
            wit::Encoding::Base64 => {
                base64::encoded_len(MAX_ASSET_BYTES).expect("bounded asset length")
            }
        };
        if bytes.len() > CHUNK_BYTES || next.is_none_or(|len| len > stored_limit) {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::TooLarge,
                "asset write exceeds its byte ceiling",
            )));
        }
        let previous_decoded = writer.validator.decoded_len();
        if matches!(writer.encoding, wit::Encoding::Base64) && {
            let started = std::time::Instant::now();
            let span = tracing::info_span!(
                "asset.decode",
                bytes = bytes.len(),
                duration_us = tracing::field::Empty
            );
            let invalid = span.in_scope(|| writer.validator.write(&bytes).is_err());
            span.record("duration_us", started.elapsed().as_micros() as u64);
            invalid
        } {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::InvalidEncoding,
                "asset writer is not canonical base64",
            )));
        }
        let (decoded, added) = match writer.encoding {
            wit::Encoding::Identity => (next.expect("stored length checked"), bytes.len() as u64),
            wit::Encoding::Base64 => (
                writer.validator.decoded_len(),
                writer.validator.decoded_len() - previous_decoded,
            ),
        };
        let total = self.assets.written.checked_add(added);
        if decoded > MAX_ASSET_BYTES || total.is_none_or(|len| len > MAX_INVOCATION_BYTES) {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::TooLarge,
                "asset write exceeds its decoded byte ceiling",
            )));
        }
        self.assets.written = total.expect("invocation length checked");
        match spool.write(bytes).await {
            Ok(spool) => {
                writer.spool = Some(spool);
                Ok(Ok(()))
            }
            Err(failure) => Ok(self.assets.refuse(io_error(failure))),
        }
    }
    async fn drop(&mut self, resource: Resource<WriterResource>) -> wasmtime::Result<()> {
        self.table.delete(resource)?;
        Ok(())
    }
}

impl wit::Host for StoreState {
    async fn open(
        &mut self,
        text: String,
    ) -> wasmtime::Result<Result<Resource<HandleResource>, wit::Error>> {
        self.assets.require_active()?;
        let Some(id) = reference(&text) else {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::UnknownReference,
                "reference was not passed with this invocation",
            )));
        };
        let Some((_, file)) = self.assets.inputs.iter().find(|(key, _)| key.0 == id) else {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::UnknownReference,
                "reference was not passed with this invocation",
            )));
        };
        if self.assets.outputs.removed.contains(&id) {
            return Ok(self
                .assets
                .refuse(error(wit::ErrorCode::UnknownReference, "asset was removed")));
        }
        let Some(row) = self.assets.rows.iter().find(|row| row.id == id) else {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::UnknownReference,
                "reference has no table row",
            )));
        };
        let info = row_info(row);
        let fd = file.clone();
        let len = match decoded_length(fd.clone(), info.encoding, row.bytes).await {
            Ok(len) => len,
            Err(failure) => return Ok(self.assets.refuse(failure)),
        };
        tracing::info!(
            "asset.id" = id,
            "asset.content_type" = info.content_type,
            "asset.bytes" = row.bytes,
            "asset opened"
        );
        Ok(Ok(self.table.push(HandleResource {
            digest_recorded: false,
            source: Source::File { fd, cursor: 0, len },
            info,
        })?))
    }
    async fn allocate(
        &mut self,
        content_type: String,
        encoding: wit::Encoding,
    ) -> wasmtime::Result<Result<Resource<WriterResource>, wit::Error>> {
        self.assets.require_active()?;
        let Some(directory) = &self.assets.directory else {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::Unconfigured,
                "configure assets.rootPath before allocating assets",
            )));
        };
        let spool = match directory.allocate().await {
            Ok(spool) => spool,
            Err(failure) => return Ok(self.assets.refuse(io_error(failure))),
        };
        let sink = WriterSink::File(spool);
        #[cfg(test)]
        let sink = match &self.assets.channel_sink {
            Some(sender) => WriterSink::Channel {
                sender: sender.clone(),
                bytes: 0,
            },
            None => sink,
        };
        Ok(Ok(self.table.push(WriterResource {
            spool: Some(sink),
            content_type,
            encoding,
            validator: Validator::default(),
        })?))
    }
    async fn attach(
        &mut self,
        resource: Resource<WriterResource>,
    ) -> wasmtime::Result<Result<Resource<HandleResource>, wit::Error>> {
        self.assets.require_active()?;
        let writer = self.table.delete(resource)?;
        if !self.assets.grant.attach {
            return Ok(self
                .assets
                .refuse(error(wit::ErrorCode::Denied, "asset.attach is not granted")));
        }
        if self.assets.outputs.attached.len() >= MAX_DESCRIPTORS_PER_FRAME {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::TooManyAssets,
                "at most five assets may be attached",
            )));
        }
        if matches!(writer.encoding, wit::Encoding::Base64) && writer.validator.finish().is_err() {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::InvalidEncoding,
                "asset writer has an invalid final quantum",
            )));
        }
        let Some(spool) = writer.spool else {
            return Ok(self
                .assets
                .refuse(error(wit::ErrorCode::Io, "asset writer is closed")));
        };
        let bytes = spool.len();
        let file = match spool.finish().await {
            Ok(file) => file,
            Err(failure) => return Ok(self.assets.refuse(io_error(failure))),
        };
        let handle = match HandleResource::from_output(
            file,
            writer.content_type.clone(),
            writer.encoding,
            self.assets.origin.clone(),
        )
        .await
        {
            Ok(handle) => handle,
            Err(failure) => return Ok(self.assets.refuse(failure)),
        };
        let digest = match handle.digest().await {
            Ok(digest) => digest,
            Err(failure) => return Ok(self.assets.refuse(failure)),
        };
        tracing::info!(
            "asset.id" = handle.info.id,
            "asset.content_type" = handle.info.content_type,
            "asset.bytes" = bytes,
            "asset.sha256" = digest,
            "asset attached"
        );
        let Source::File { fd, .. } = &handle.source;
        if let Some(owner) = fd.output_file() {
            self.assets.outputs.files.push(owner);
        }
        self.assets.outputs.attached.push(NewAsset {
            descriptor: self.assets.outputs.attached.len() as u32,
            content_type: writer.content_type,
            encoding: wire_encoding(writer.encoding),
            bytes,
            sha256: digest,
        });
        Ok(Ok(self.table.push(handle)?))
    }
    async fn list(&mut self) -> wasmtime::Result<Vec<wit::Info>> {
        self.assets.require_active()?;
        Ok(self
            .assets
            .rows
            .iter()
            .filter(|row| !self.assets.outputs.removed.contains(&row.id))
            .map(|row| {
                let mut info = row_info(row);
                info.sent |= self.assets.outputs.sent.contains(&row.id);
                info
            })
            .collect())
    }
    async fn remove(
        &mut self,
        resource: Resource<HandleResource>,
    ) -> wasmtime::Result<Result<(), wit::Error>> {
        self.assets.require_active()?;
        if !self.assets.grant.remove {
            return Ok(self
                .assets
                .refuse(error(wit::ErrorCode::Denied, "asset.remove is not granted")));
        }
        let info = &self.table.get(&resource)?.info;
        let Some(id) = info.id else {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::UnknownReference,
                "asset has not joined the conversation table",
            )));
        };
        if info.sent || self.assets.outputs.sent.contains(&id) {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::AlreadySent,
                "sent assets cannot be removed",
            )));
        }
        if !self.assets.outputs.removed.contains(&id) {
            self.assets.outputs.removed.push(id);
        }
        Ok(Ok(()))
    }
    async fn send(
        &mut self,
        resource: Resource<HandleResource>,
    ) -> wasmtime::Result<Result<(), wit::Error>> {
        self.assets.require_active()?;
        if !self.assets.grant.send {
            return Ok(self
                .assets
                .refuse(error(wit::ErrorCode::Denied, "asset.send is not granted")));
        }
        let info = &self.table.get(&resource)?.info;
        let Some(id) = info.id else {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::UnknownReference,
                "asset has not joined the conversation table",
            )));
        };
        if self.assets.outputs.removed.contains(&id) {
            return Ok(self
                .assets
                .refuse(error(wit::ErrorCode::UnknownReference, "asset was removed")));
        }
        if info.sent || self.assets.outputs.sent.contains(&id) {
            return Ok(Ok(()));
        }
        if self.assets.sends_remaining == 0 {
            return Ok(self.assets.refuse(error(
                wit::ErrorCode::SendsExhausted,
                "asset send allowance is exhausted",
            )));
        }
        self.assets.sends_remaining -= 1;
        self.assets.outputs.sent.push(id);
        Ok(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_limits_match_the_shared_contract() {
        assert_eq!(
            MAX_ASSET_BYTES,
            dekopon_core::asset::MAX_DECODED_ASSET_BYTES as u64
        );
        assert_eq!(
            MAX_INVOCATION_BYTES,
            dekopon_core::asset::MAX_DECODED_INVOCATION_BYTES as u64
        );
    }

    use crate::{
        BrokerHostLimits, BrokerHostOptions, Runtime, clock::ClockState, http::HttpState,
        storage::StorageState,
    };
    use std::time::Duration;
    use wit::{Host as _, HostHandle as _, HostWriter as _};

    async fn state(
        directory: Option<AssetDirectory>,
        grant: AssetConstraints,
        inputs: AssetInputs,
        refs: Vec<u64>,
    ) -> StoreState {
        let runtime =
            Runtime::new(BrokerHostLimits::default(), &BrokerHostOptions::default()).unwrap();
        let http = HttpState::describe(runtime.http_ceilings(), Duration::from_secs(5)).unwrap();
        let mut state = runtime
            .store(http, StorageState::disabled(), ClockState::invoke())
            .unwrap()
            .into_data();
        state.assets = AssetState::invoke(
            inputs,
            refs,
            grant,
            directory,
            "provider:probe.attach".to_owned(),
        )
        .await
        .unwrap();
        state
    }

    fn input(file: File, bytes: u64, sent: bool) -> AssetInputs {
        AssetInputs {
            rows: vec![AssetRow {
                id: 1,
                content_type: "text/plain".to_owned(),
                encoding: AssetEncoding::Identity,
                bytes,
                origin: "chat".to_owned(),
                sent,
            }],
            descriptors: vec![file.into()],
            sends_remaining: 1,
        }
    }

    #[tokio::test]
    async fn duplicated_descriptors_and_reopened_handles_have_independent_positional_cursors() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"abcdef").unwrap();
        let reader = File::open(file.path()).unwrap();
        let gateway = reader.try_clone().unwrap();
        file.close().unwrap();
        let mut state = state(
            None,
            AssetConstraints::default(),
            input(reader, 6, false),
            vec![1],
        )
        .await;
        let first = state
            .open("chat-asset:1".to_owned())
            .await
            .unwrap()
            .unwrap();
        let second = state
            .open("chat-asset:1".to_owned())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state
                .read(Resource::new_borrow(first.rep()), 2)
                .await
                .unwrap()
                .unwrap(),
            b"ab"
        );
        let mut gateway_bytes = [0; 6];
        gateway.read_exact_at(&mut gateway_bytes, 0).unwrap();
        assert_eq!(&gateway_bytes, b"abcdef");
        assert_eq!(
            state
                .read(Resource::new_borrow(first.rep()), 2)
                .await
                .unwrap()
                .unwrap(),
            b"cd"
        );
        assert_eq!(
            state
                .read(Resource::new_borrow(second.rep()), 6)
                .await
                .unwrap()
                .unwrap(),
            b"abcdef"
        );
        assert_eq!(
            state
                .read_at(Resource::new_borrow(first.rep()), 1, 3)
                .await
                .unwrap()
                .unwrap(),
            b"bcd"
        );
        assert_eq!(
            state
                .read(Resource::new_borrow(first.rep()), 6)
                .await
                .unwrap()
                .unwrap(),
            b"ef"
        );
        drop(state);
        let mut later = self::state(
            None,
            AssetConstraints::default(),
            input(gateway.try_clone().unwrap(), 6, false),
            vec![1],
        )
        .await;
        let handle = later
            .open("chat-asset:1".to_owned())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(later.read(handle, 6).await.unwrap().unwrap(), b"abcdef");
    }

    #[tokio::test]
    async fn streamed_assets_share_the_invocation_budget_and_response_replay_refuses_before_dispatch()
     {
        use crate::bindings::dekopon::http::client as http;
        use http::Host as _;
        let root = tempfile::tempdir().unwrap();
        let input_file = tempfile::NamedTempFile::new().unwrap();
        input_file.as_file().set_len(MAX_ASSET_BYTES).unwrap();
        let mut state = state(
            Some(AssetDirectory::new(root.path().to_owned(), 1024)),
            AssetConstraints::default(),
            input(
                File::open(input_file.path()).unwrap(),
                MAX_ASSET_BYTES,
                false,
            ),
            vec![1],
        )
        .await;
        let server = dekopon_test_support::LoopbackServer::serving(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx",
            2,
        );
        state.http = HttpState::invoke(
            Some(dekopon_capability::HttpConstraints {
                allowed_hosts: vec![server.authority().to_owned()],
                allowed_methods: vec!["POST".to_owned()],
                max_requests: 3,
                max_request_bytes: 1024,
                max_response_bytes: 1024,
                allow_plaintext_loopback: true,
            }),
            None,
            None,
            Default::default(),
            Duration::from_secs(30),
        )
        .unwrap();
        let handle = state
            .open("chat-asset:1".to_owned())
            .await
            .unwrap()
            .unwrap();
        let request = |handle, count| http::StreamedRequest {
            method: "POST".to_owned(),
            uri: server.url(),
            headers: vec![],
            body: (0..count)
                .map(|_| {
                    http::Part::Asset(http::AssetPart {
                        handle: Resource::new_borrow(handle),
                        encoding: wit::Encoding::Base64,
                    })
                })
                .collect(),
        };
        state
            .stream(request(handle.rep(), 4))
            .await
            .unwrap()
            .unwrap();
        drop(server.request());
        let response = state
            .stream(request(handle.rep(), 1))
            .await
            .unwrap()
            .unwrap();
        drop(server.request());
        assert_eq!(state.assets.written, MAX_INVOCATION_BYTES);
        let error = state
            .stream(request(response.body.rep(), 1))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, http::ErrorCode::RequestTooLarge);
        assert_eq!(state.assets.violation(), Some(wit::ErrorCode::TooLarge));
        assert_eq!(state.assets.written, MAX_INVOCATION_BYTES);
        assert_eq!(state.http.into_evidence().len(), 2);
        assert!(server.recorded().is_empty());
        server.join();
    }

    #[tokio::test]
    async fn streamed_assets_and_writers_charge_the_same_decoded_counter() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(
            Some(AssetDirectory::new(root.path().to_owned(), 1024)),
            AssetConstraints::default(),
            AssetInputs::default(),
            vec![],
        )
        .await;
        let writer = state
            .allocate("text/plain".to_owned(), wit::Encoding::Identity)
            .await
            .unwrap()
            .unwrap();
        state
            .assets
            .charge_stream(MAX_INVOCATION_BYTES - 1)
            .unwrap();
        state
            .write(Resource::new_borrow(writer.rep()), vec![b'x'])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.assets.written, MAX_INVOCATION_BYTES);
        assert_eq!(
            state.assets.charge_stream(1).unwrap_err().code,
            wit::ErrorCode::TooLarge
        );
        assert_eq!(
            state
                .write(Resource::new_borrow(writer.rep()), vec![b'x'])
                .await
                .unwrap()
                .unwrap_err()
                .code,
            wit::ErrorCode::TooLarge
        );
    }

    #[tokio::test]
    async fn allocation_requires_configuration_and_pure_contexts_trap() {
        let mut state = state(
            None,
            AssetConstraints::default(),
            AssetInputs::default(),
            vec![],
        )
        .await;
        let error = state
            .allocate("text/plain".to_owned(), wit::Encoding::Identity)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, wit::ErrorCode::Unconfigured);
        assert!(error.message.contains("assets.rootPath"));
        state.assets = AssetState::disabled();
        assert!(state.list().await.is_err());
        assert!(state.assets.attempted());
    }

    #[tokio::test]
    async fn a_writer_is_bounded_at_64_kib_per_call_and_eight_mib_total() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), MAX_INVOCATION_BYTES);
        let mut state = state(
            Some(directory),
            AssetConstraints {
                attach: true,
                ..Default::default()
            },
            AssetInputs::default(),
            vec![],
        )
        .await;
        let writer = state
            .allocate("text/plain".to_owned(), wit::Encoding::Identity)
            .await
            .unwrap()
            .unwrap();
        for _ in 0..MAX_ASSET_BYTES / CHUNK_BYTES as u64 {
            state
                .write(Resource::new_borrow(writer.rep()), vec![b'a'; CHUNK_BYTES])
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            state
                .table
                .get(&writer)
                .unwrap()
                .spool
                .as_ref()
                .unwrap()
                .len(),
            MAX_ASSET_BYTES
        );
        assert_eq!(
            state
                .write(Resource::new_borrow(writer.rep()), vec![b'!'])
                .await
                .unwrap()
                .unwrap_err()
                .code,
            wit::ErrorCode::TooLarge
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let writer = state
            .allocate("text/plain".to_owned(), wit::Encoding::Identity)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state
                .write(Resource::new_borrow(writer.rep()), vec![0; CHUNK_BYTES + 1])
                .await
                .unwrap()
                .unwrap_err()
                .code,
            wit::ErrorCode::TooLarge
        );
    }

    #[tokio::test]
    async fn base64_writer_limits_count_decoded_bytes_including_padding() {
        let stored_limit = base64::encoded_len(MAX_ASSET_BYTES).unwrap();
        assert_eq!(stored_limit, 11_184_812);
        for extra in [0, 1] {
            let root = tempfile::tempdir().unwrap();
            let directory = AssetDirectory::new(root.path().to_owned(), stored_limit + 1);
            let mut state = state(
                Some(directory),
                AssetConstraints {
                    attach: true,
                    ..Default::default()
                },
                AssetInputs::default(),
                vec![],
            )
            .await;
            let writer = state
                .allocate("image/png".to_owned(), wit::Encoding::Base64)
                .await
                .unwrap()
                .unwrap();
            let encoded = base64::Engine::encode(
                &base64::STANDARD,
                vec![0; MAX_ASSET_BYTES as usize + extra],
            );
            // The one-decoded-byte overflow has the SAME stored length, but different padding.
            assert_eq!(encoded.len() as u64, stored_limit);
            let mut result = Ok(());
            for chunk in encoded.as_bytes().chunks(CHUNK_BYTES - 1) {
                result = state
                    .write(Resource::new_borrow(writer.rep()), chunk.to_vec())
                    .await
                    .unwrap();
                if result.is_err() {
                    break;
                }
            }
            if extra == 0 {
                result.unwrap();
                assert_eq!(state.assets.written, MAX_ASSET_BYTES);
                assert_eq!(
                    state
                        .write(Resource::new_borrow(writer.rep()), vec![b'A'])
                        .await
                        .unwrap()
                        .unwrap_err()
                        .code,
                    wit::ErrorCode::TooLarge
                );
            } else {
                assert_eq!(result.unwrap_err().code, wit::ErrorCode::TooLarge);
            }
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn base64_invocation_decoded_sum_accepts_forty_mib_then_refuses_one_byte() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 64 * 1024 * 1024);
        let mut state = state(
            Some(directory),
            AssetConstraints::default(),
            AssetInputs::default(),
            vec![],
        )
        .await;
        let encoded = base64::Engine::encode(&base64::STANDARD, vec![0; MAX_ASSET_BYTES as usize]);
        for _ in 0..5 {
            let writer = state
                .allocate("image/png".to_owned(), wit::Encoding::Base64)
                .await
                .unwrap()
                .unwrap();
            for chunk in encoded.as_bytes().chunks(CHUNK_BYTES - 1) {
                state
                    .write(Resource::new_borrow(writer.rep()), chunk.to_vec())
                    .await
                    .unwrap()
                    .unwrap();
            }
            wit::HostWriter::drop(&mut state, writer).await.unwrap();
        }
        assert_eq!(state.assets.written, MAX_INVOCATION_BYTES);
        let writer = state
            .allocate("text/plain".to_owned(), wit::Encoding::Base64)
            .await
            .unwrap()
            .unwrap();
        state
            .write(Resource::new_borrow(writer.rep()), b"AA=".to_vec())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state
                .write(Resource::new_borrow(writer.rep()), b"=".to_vec())
                .await
                .unwrap()
                .unwrap_err()
                .code,
            wit::ErrorCode::TooLarge
        );
    }

    #[tokio::test]
    async fn base64_input_sum_accepts_forty_decoded_mib_and_refuses_one_more() {
        use std::io::Write as _;
        for extra in [0, 1] {
            let mut inputs = AssetInputs::default();
            for id in 1..=5 {
                let decoded = MAX_ASSET_BYTES as usize + if id == 5 { extra } else { 0 };
                let encoded = base64::Engine::encode(&base64::STANDARD, vec![0; decoded]);
                let mut file = tempfile::NamedTempFile::new().unwrap();
                file.write_all(encoded.as_bytes()).unwrap();
                inputs.rows.push(AssetRow {
                    id,
                    content_type: "image/png".to_owned(),
                    encoding: AssetEncoding::Base64,
                    bytes: encoded.len() as u64,
                    origin: "test".to_owned(),
                    sent: false,
                });
                inputs
                    .descriptors
                    .push(File::open(file.path()).unwrap().into());
            }
            let result = AssetState::invoke(
                inputs,
                vec![1, 2, 3, 4, 5],
                AssetConstraints::default(),
                None,
                "test".to_owned(),
            )
            .await;
            if extra == 0 {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(AssetAdmissionError::TooLarge)));
            }
        }
    }

    #[tokio::test]
    async fn five_attachments_fit_the_frame_and_the_sixth_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), MAX_INVOCATION_BYTES);
        let mut state = state(
            Some(directory),
            AssetConstraints {
                attach: true,
                ..Default::default()
            },
            AssetInputs::default(),
            vec![],
        )
        .await;
        for index in 0..=MAX_DESCRIPTORS_PER_FRAME {
            let writer = state
                .allocate("text/plain".to_owned(), wit::Encoding::Identity)
                .await
                .unwrap()
                .unwrap();
            let attached = state.attach(writer).await.unwrap();
            if index == MAX_DESCRIPTORS_PER_FRAME {
                assert_eq!(attached.unwrap_err().code, wit::ErrorCode::TooManyAssets);
            } else {
                let handle = attached.unwrap();
                assert_eq!(
                    state.info(handle).await.unwrap().origin,
                    "provider:probe.attach"
                );
                assert_eq!(
                    state.assets.outputs.attached[index].descriptor,
                    index as u32
                );
            }
        }
        assert_eq!(state.assets.outputs.files.len(), MAX_DESCRIPTORS_PER_FRAME);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn total_writer_bytes_accept_forty_mib_and_refuse_one_more() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), MAX_INVOCATION_BYTES + 1);
        let mut state = state(
            Some(directory),
            AssetConstraints::default(),
            AssetInputs::default(),
            vec![],
        )
        .await;
        for _ in 0..5 {
            let writer = state
                .allocate("text/plain".to_owned(), wit::Encoding::Identity)
                .await
                .unwrap()
                .unwrap();
            for _ in 0..MAX_ASSET_BYTES / CHUNK_BYTES as u64 {
                state
                    .write(Resource::new_borrow(writer.rep()), vec![0; CHUNK_BYTES])
                    .await
                    .unwrap()
                    .unwrap();
            }
            wit::HostWriter::drop(&mut state, writer).await.unwrap();
        }
        assert_eq!(state.assets.written, MAX_INVOCATION_BYTES);
        let writer = state
            .allocate("text/plain".to_owned(), wit::Encoding::Identity)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state
                .write(Resource::new_borrow(writer.rep()), vec![0])
                .await
                .unwrap()
                .unwrap_err()
                .code,
            wit::ErrorCode::TooLarge
        );
    }

    #[tokio::test]
    async fn attach_hashes_decoded_bytes_and_reads_at_decoded_offsets() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 100);
        let mut state = state(
            Some(directory),
            AssetConstraints {
                attach: true,
                ..Default::default()
            },
            AssetInputs::default(),
            vec![],
        )
        .await;
        let writer = state
            .allocate("text/plain".to_owned(), wit::Encoding::Base64)
            .await
            .unwrap()
            .unwrap();
        for bytes in [b"YWJ".as_slice(), b"jZGVm"] {
            state
                .write(Resource::new_borrow(writer.rep()), bytes.to_vec())
                .await
                .unwrap()
                .unwrap();
        }
        let handle = state.attach(writer).await.unwrap().unwrap();
        assert_eq!(
            state
                .read_at(Resource::new_borrow(handle.rep()), 1, 4)
                .await
                .unwrap()
                .unwrap(),
            b"bcde"
        );
        assert_eq!(
            state.assets.outputs.attached[0].sha256,
            Sha256::digest(b"abcdef")
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        assert!(
            state.assets.outputs.files[0]
                .file()
                .write_at(b"x", 0)
                .is_err()
        );
        let writer = state
            .allocate("text/plain".to_owned(), wit::Encoding::Base64)
            .await
            .unwrap()
            .unwrap();
        state
            .write(Resource::new_borrow(writer.rep()), b"Zg=".to_vec())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state.attach(writer).await.unwrap().unwrap_err().code,
            wit::ErrorCode::InvalidEncoding
        );
    }

    #[tokio::test]
    async fn grants_send_allowance_duplicate_send_and_sent_removal_are_enforced() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut state = state(
            None,
            AssetConstraints::default(),
            input(File::open(file.path()).unwrap(), 0, false),
            vec![1],
        )
        .await;
        let handle = state
            .open("chat-asset:1".to_owned())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state
                .send(Resource::new_borrow(handle.rep()))
                .await
                .unwrap()
                .unwrap_err()
                .code,
            wit::ErrorCode::Denied
        );
        state.assets.grant.send = true;
        state.assets.sends_remaining = 0;
        assert_eq!(
            state
                .send(Resource::new_borrow(handle.rep()))
                .await
                .unwrap()
                .unwrap_err()
                .code,
            wit::ErrorCode::SendsExhausted
        );
        state.assets.sends_remaining = 1;
        state
            .send(Resource::new_borrow(handle.rep()))
            .await
            .unwrap()
            .unwrap();
        state
            .send(Resource::new_borrow(handle.rep()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.assets.outputs.sent, vec![1]);
        assert_eq!(state.assets.sends_remaining, 0);
        state.assets.grant.remove = true;
        assert_eq!(
            state
                .remove(Resource::new_borrow(handle.rep()))
                .await
                .unwrap()
                .unwrap_err()
                .code,
            wit::ErrorCode::AlreadySent
        );
    }

    #[tokio::test]
    async fn dropping_invocation_resources_removes_unattached_files_and_releases_budget() {
        let root = tempfile::tempdir().unwrap();
        let directory = AssetDirectory::new(root.path().to_owned(), 4);
        let mut state = state(
            Some(directory.clone()),
            AssetConstraints::default(),
            AssetInputs::default(),
            vec![],
        )
        .await;
        let writer = state
            .allocate("text/plain".to_owned(), wit::Encoding::Identity)
            .await
            .unwrap()
            .unwrap();
        state
            .write(Resource::new_borrow(writer.rep()), b"four".to_vec())
            .await
            .unwrap()
            .unwrap();
        drop(state);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        directory
            .allocate()
            .await
            .unwrap()
            .write(b"four".to_vec())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn writable_descriptors_are_refused_before_guest_execution() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(
            AssetState::invoke(
                input(file.reopen().unwrap(), 0, false),
                vec![1],
                AssetConstraints::default(),
                None,
                "provider:probe".to_owned()
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn reference_discovery_is_distinct_ordered_and_bounded_at_the_first_overflow() {
        assert_eq!(
            references(&serde_json::json!([
                "chat-asset:2",
                "chat-asset:1",
                "chat-asset:2",
                "chat-asset:01",
                "chat-asset:0"
            ])),
            vec![2, 1]
        );
        assert_eq!(
            references(&serde_json::json!([
                "chat-asset:1",
                "chat-asset:2",
                "chat-asset:3",
                "chat-asset:4",
                "chat-asset:5"
            ]))
            .len(),
            MAX_DESCRIPTORS_PER_FRAME
        );
        let many = serde_json::Value::Array(
            (1..1000)
                .map(|id| serde_json::json!(format!("chat-asset:{id}")))
                .collect(),
        );
        assert_eq!(references(&many).len(), MAX_DESCRIPTORS_PER_FRAME + 1);
    }
    #[tokio::test]
    async fn a_capacity_one_channel_sink_suspends_and_resumes_the_real_guest_without_changing_wit()
    {
        let root = tempfile::tempdir().unwrap();
        let registry = crate::BrokerProviderRegistry::load(
            [dekopon_test_support::provider_fixture(
                "http-probe-provider.wasm",
            )],
            BrokerHostLimits::default(),
        )
        .await
        .unwrap();
        let provider = &registry.providers[0];
        let http =
            HttpState::describe(provider.runtime.http_ceilings(), Duration::from_secs(5)).unwrap();
        let mut store = provider
            .runtime
            .store(http, StorageState::disabled(), ClockState::invoke())
            .unwrap();
        store.data_mut().assets = AssetState::invoke(
            AssetInputs::default(),
            vec![],
            AssetConstraints::default(),
            Some(AssetDirectory::new(root.path().to_owned(), 100)),
            "provider:http-probe.fetch".to_owned(),
        )
        .await
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        store.data_mut().assets.channel_sink = Some(sender);
        let capability = "http-probe.fetch".parse().unwrap();
        let constraints = dekopon_capability::ExecutionConstraints::default();
        let call = provider.execute_in_store(
            &mut store,
            &capability,
            r#"{"assetMode":"channel"}"#,
            &constraints,
            Duration::from_secs(5),
        );
        tokio::pin!(call);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut call)
                .await
                .is_err()
        );
        assert_eq!(receiver.recv().await.unwrap(), b"asset ");
        assert_eq!(call.await.unwrap(), serde_json::json!({"ok": true}));
        assert_eq!(receiver.recv().await.unwrap(), b"probe");
    }
    #[tokio::test]
    async fn input_rows_accept_thirty_two_and_refuse_thirty_three() {
        for count in [MAX_ASSET_ROWS, MAX_ASSET_ROWS + 1] {
            let inputs = AssetInputs {
                rows: (1..=count as u64)
                    .map(|id| AssetRow {
                        id,
                        content_type: "text/plain".to_owned(),
                        encoding: AssetEncoding::Identity,
                        bytes: 0,
                        origin: "chat".to_owned(),
                        sent: false,
                    })
                    .collect(),
                ..Default::default()
            };
            let admitted = AssetState::invoke(
                inputs,
                vec![],
                AssetConstraints::default(),
                None,
                "provider:probe".to_owned(),
            )
            .await;
            if count == MAX_ASSET_ROWS {
                assert!(admitted.is_ok());
            } else {
                assert!(matches!(admitted, Err(AssetAdmissionError::TooManyRows)));
            }
        }
    }

    #[tokio::test]
    async fn input_descriptor_lengths_accept_forty_mib_and_refuse_one_more() {
        for bytes in [MAX_INVOCATION_BYTES, MAX_INVOCATION_BYTES + 1] {
            let file = tempfile::NamedTempFile::new().unwrap();
            file.as_file().set_len(bytes).unwrap();
            let admitted = AssetState::invoke(
                input(File::open(file.path()).unwrap(), bytes, false),
                vec![1],
                AssetConstraints::default(),
                None,
                "provider:probe".to_owned(),
            )
            .await;
            if bytes == MAX_INVOCATION_BYTES {
                assert!(admitted.is_ok());
            } else {
                assert!(matches!(admitted, Err(AssetAdmissionError::TooLarge)));
            }
        }
    }

    #[tokio::test]
    async fn attach_without_a_grant_is_denied_and_destroys_the_consumed_writer() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(
            Some(AssetDirectory::new(root.path().to_owned(), 1)),
            AssetConstraints::default(),
            AssetInputs::default(),
            vec![],
        )
        .await;
        let writer = state
            .allocate("text/plain".to_owned(), wit::Encoding::Identity)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state.attach(writer).await.unwrap().unwrap_err().code,
            wit::ErrorCode::Denied
        );
        assert!(state.assets.outputs.attached.is_empty());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[tokio::test]
    async fn host_reads_clamp_one_past_64_kib_and_preserve_the_remaining_byte() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len((CHUNK_BYTES + 1) as u64).unwrap();
        let mut state = state(
            None,
            AssetConstraints::default(),
            input(
                File::open(file.path()).unwrap(),
                (CHUNK_BYTES + 1) as u64,
                false,
            ),
            vec![1],
        )
        .await;
        let handle = state
            .open("chat-asset:1".to_owned())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state
                .read(Resource::new_borrow(handle.rep()), (CHUNK_BYTES + 1) as u32)
                .await
                .unwrap()
                .unwrap()
                .len(),
            CHUNK_BYTES
        );
        assert_eq!(
            state
                .read(Resource::new_borrow(handle.rep()), 1)
                .await
                .unwrap()
                .unwrap()
                .len(),
            1
        );
    }
}
