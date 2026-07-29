//! Bounded local broker wire protocol and unprivileged Unix-socket client.
//!
//! Wire requests carry no trusted identity or authorization. A server derives
//! `dekopon_broker::AuthenticatedContext` from operating-system peer credentials and trusted
//! mapping before dispatching these untrusted requests.

#![forbid(unsafe_code)]

use std::{fmt, io, time::Duration};

#[cfg(unix)]
use std::{
    os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

use dekopon_broker::{AvailableCapability, InvocationRequest};
use dekopon_capability::InvocationResult;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    time::timeout,
};

#[cfg(unix)]
use tokio::net::UnixStream;

/// Initial local broker protocol identifier.
pub const PROTOCOL_VERSION: &str = "dekopon.dev/broker/v1alpha1";
/// Default complete request/response frame bound (1 MiB).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Hard ceiling accepted for any configured frame bound (16 MiB).
pub const HARD_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Default connection/read/write deadline.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Exact protocol version carried by every envelope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProtocolVersion {
    /// Initial strict JSON protocol.
    #[serde(rename = "dekopon.dev/broker/v1alpha1")]
    V1Alpha1,
}

/// One strict untrusted client request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RequestEnvelope {
    /// Exact wire protocol version.
    pub api_version: ProtocolVersion,
    /// Requested operation.
    pub request: BrokerRequest,
}

impl RequestEnvelope {
    /// Creates a capability-inspection request.
    #[must_use]
    pub const fn capabilities() -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            request: BrokerRequest::Capabilities,
        }
    }

    /// Creates an untrusted invocation proposal request.
    #[must_use]
    pub const fn invoke(invocation: InvocationRequest) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            request: BrokerRequest::Invoke { invocation },
        }
    }
}

/// Operations accepted by the local broker.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "operation", deny_unknown_fields, rename_all = "camelCase")]
pub enum BrokerRequest {
    /// Lists capabilities allowed for the authenticated peer context.
    Capabilities,
    /// Submits one untrusted invocation proposal.
    Invoke {
        /// Proposal fields without principal or actor claims.
        invocation: InvocationRequest,
    },
}

/// One strict public broker response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ResponseEnvelope {
    /// Exact wire protocol version.
    pub api_version: ProtocolVersion,
    /// Operation result.
    pub response: BrokerResponse,
}

impl ResponseEnvelope {
    /// Creates a successful capability response.
    #[must_use]
    pub const fn capabilities(capabilities: Vec<AvailableCapability>) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            response: BrokerResponse::Capabilities { capabilities },
        }
    }

    /// Creates a completed invocation response, including denials and provider failures.
    #[must_use]
    pub const fn invocation(result: InvocationResult) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            response: BrokerResponse::Invocation { result },
        }
    }

    /// Creates a stable protocol/server failure response without internal details.
    #[must_use]
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha1,
            response: BrokerResponse::Error {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

/// Public response variants.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", deny_unknown_fields, rename_all = "camelCase")]
pub enum BrokerResponse {
    /// Capabilities visible under exact policy for the authenticated peer.
    Capabilities {
        /// Deterministically sorted capabilities.
        capabilities: Vec<AvailableCapability>,
    },
    /// Terminal invocation result.
    Invocation {
        /// Denied, failed, or succeeded result with public evidence.
        result: InvocationResult,
    },
    /// Protocol or broker infrastructure failure.
    Error {
        /// Stable public machine code.
        code: String,
        /// Bounded non-sensitive message.
        message: String,
    },
}

/// Independent bound and deadline for one complete frame operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameLimits {
    /// Maximum JSON payload bytes, excluding the four-byte prefix.
    pub max_frame_bytes: usize,
    /// Deadline for a complete prefix+payload read or write.
    pub io_timeout: Duration,
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            io_timeout: DEFAULT_IO_TIMEOUT,
        }
    }
}

impl FrameLimits {
    /// Validates a configured bound before any I/O or allocation.
    pub fn validate(self) -> Result<Self, ProtocolError> {
        if self.max_frame_bytes == 0 || self.max_frame_bytes > HARD_MAX_FRAME_BYTES {
            return Err(ProtocolError::InvalidFrameLimit {
                maximum: HARD_MAX_FRAME_BYTES,
            });
        }
        if self.io_timeout.is_zero() {
            return Err(ProtocolError::ZeroTimeout);
        }
        Ok(self)
    }
}

/// Reads and strictly decodes one complete length-delimited JSON frame.
pub async fn read_frame<R, T>(reader: &mut R, limits: FrameLimits) -> Result<T, ProtocolError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let limits = limits.validate()?;
    let bytes = timeout(limits.io_timeout, async {
        let mut prefix = [0_u8; 4];
        reader.read_exact(&mut prefix).await?;
        let length = usize::try_from(u32::from_be_bytes(prefix)).unwrap_or(usize::MAX);
        if length == 0 {
            return Err(ReadFrameError::Empty);
        }
        if length > limits.max_frame_bytes {
            return Err(ReadFrameError::TooLarge { length });
        }
        let mut bytes = vec![0_u8; length];
        reader.read_exact(&mut bytes).await?;
        Ok(bytes)
    })
    .await
    .map_err(|_| ProtocolError::Timeout)?
    .map_err(|error| match error {
        ReadFrameError::Io(source) => ProtocolError::Io { source },
        ReadFrameError::Empty => ProtocolError::EmptyFrame,
        ReadFrameError::TooLarge { length } => ProtocolError::FrameTooLarge {
            length,
            maximum: limits.max_frame_bytes,
        },
    })?;
    serde_json::from_slice(&bytes).map_err(|source| ProtocolError::Deserialize { source })
}

#[derive(Debug)]
enum ReadFrameError {
    Io(io::Error),
    Empty,
    TooLarge { length: usize },
}

impl From<io::Error> for ReadFrameError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

/// Strictly serializes and writes one complete length-delimited JSON frame.
pub async fn write_frame<W, T>(
    writer: &mut W,
    value: &T,
    limits: FrameLimits,
) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let limits = limits.validate()?;
    let mut buffer = BoundedJsonBuffer::new(limits.max_frame_bytes);
    if let Err(source) = serde_json::to_writer(&mut buffer, value) {
        if buffer.exceeded {
            return Err(ProtocolError::FrameTooLarge {
                length: limits.max_frame_bytes.saturating_add(1),
                maximum: limits.max_frame_bytes,
            });
        }
        return Err(ProtocolError::Serialize { source });
    }
    let length = u32::try_from(buffer.bytes.len()).map_err(|_| ProtocolError::FrameTooLarge {
        length: buffer.bytes.len(),
        maximum: limits.max_frame_bytes,
    })?;
    timeout(limits.io_timeout, async {
        writer.write_all(&length.to_be_bytes()).await?;
        writer.write_all(&buffer.bytes).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| ProtocolError::Timeout)?
    .map_err(|source| ProtocolError::Io { source })
}

struct BoundedJsonBuffer {
    bytes: Vec<u8>,
    maximum: usize,
    exceeded: bool,
}

impl BoundedJsonBuffer {
    fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(maximum.min(8 * 1024)),
            maximum,
            exceeded: false,
        }
    }
}

impl io::Write for BoundedJsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.bytes.len().checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON frame overflowed"));
        };
        if length > self.maximum {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON frame exceeded its limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Bounded framing or strict JSON failure.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// Configured frame maximum was zero or exceeded the hard ceiling.
    #[error("frame maximum must be between 1 and {maximum} bytes")]
    InvalidFrameLimit {
        /// Hard maximum.
        maximum: usize,
    },
    /// Configured I/O timeout was zero.
    #[error("frame I/O timeout must be greater than zero")]
    ZeroTimeout,
    /// Complete frame deadline expired.
    #[error("broker frame I/O timed out")]
    Timeout,
    /// Prefix or payload I/O failed.
    #[error("broker frame I/O failed")]
    Io {
        /// I/O failure.
        #[source]
        source: io::Error,
    },
    /// Prefix declared an empty JSON payload.
    #[error("broker frame must not be empty")]
    EmptyFrame,
    /// Prefix or serialization exceeded the configured frame bound.
    #[error("broker frame is {length} bytes; maximum is {maximum}")]
    FrameTooLarge {
        /// Actual or minimum known size.
        length: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Public response/request could not be serialized.
    #[error("could not serialize broker frame")]
    Serialize {
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// Frame was malformed, used an unknown version/variant, or had unknown fields.
    #[error("broker frame is not valid protocol JSON")]
    Deserialize {
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
}

/// Unprivileged one-request-per-connection Unix client.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct BrokerClient {
    socket: PathBuf,
    expected_server_uid: u32,
    limits: FrameLimits,
}

#[cfg(unix)]
impl BrokerClient {
    /// Creates a client that authenticates the server by socket metadata and peer UID.
    pub fn new(
        socket: impl Into<PathBuf>,
        expected_server_uid: u32,
        limits: FrameLimits,
    ) -> Result<Self, ClientError> {
        Ok(Self {
            socket: socket.into(),
            expected_server_uid,
            limits: limits.validate().map_err(ClientError::Protocol)?,
        })
    }

    /// Returns capabilities exact policy makes visible to this authenticated peer.
    pub async fn capabilities(&self) -> Result<Vec<AvailableCapability>, ClientError> {
        match self.exchange(RequestEnvelope::capabilities()).await? {
            BrokerResponse::Capabilities { capabilities } => Ok(capabilities),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::Invocation { .. } => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Submits untrusted invocation fields without any identity or authority claim.
    pub async fn invoke(
        &self,
        request: InvocationRequest,
    ) -> Result<InvocationResult, ClientError> {
        match self.exchange(RequestEnvelope::invoke(request)).await? {
            BrokerResponse::Invocation { result } => Ok(result),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::Capabilities { .. } => Err(ClientError::UnexpectedResponse),
        }
    }

    async fn exchange(&self, request: RequestEnvelope) -> Result<BrokerResponse, ClientError> {
        validate_socket_path(&self.socket, self.expected_server_uid).await?;
        let mut stream = timeout(self.limits.io_timeout, UnixStream::connect(&self.socket))
            .await
            .map_err(|_| ClientError::ConnectTimeout)?
            .map_err(|source| ClientError::Connect { source })?;
        let credentials = stream
            .peer_cred()
            .map_err(|source| ClientError::PeerCredentials { source })?;
        if credentials.uid() != self.expected_server_uid {
            return Err(ClientError::ServerIdentity {
                expected: self.expected_server_uid,
                actual: credentials.uid(),
            });
        }
        write_frame(&mut stream, &request, self.limits)
            .await
            .map_err(ClientError::Protocol)?;
        let response = read_frame::<_, ResponseEnvelope>(&mut stream, self.limits)
            .await
            .map_err(ClientError::Protocol)?;
        Ok(response.response)
    }
}

#[cfg(unix)]
async fn validate_socket_path(path: &Path, expected_uid: u32) -> Result<(), ClientError> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|source| ClientError::SocketMetadata { source })?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(ClientError::UnsafeSocket);
    }
    Ok(())
}

/// Client-side authentication, framing, or remote failure.
#[cfg(unix)]
#[derive(Debug, Error)]
pub enum ClientError {
    /// Socket metadata could not be inspected.
    #[error("could not inspect broker socket")]
    SocketMetadata {
        /// I/O failure.
        #[source]
        source: io::Error,
    },
    /// Socket was not a private, single-link socket owned by the expected UID.
    #[error("broker socket is not private or owned by the expected server UID")]
    UnsafeSocket,
    /// Connecting exceeded the configured deadline.
    #[error("broker connection timed out")]
    ConnectTimeout,
    /// Unix connection failed.
    #[error("could not connect to broker socket")]
    Connect {
        /// I/O failure.
        #[source]
        source: io::Error,
    },
    /// Peer credentials could not be read.
    #[error("could not authenticate broker peer credentials")]
    PeerCredentials {
        /// I/O failure.
        #[source]
        source: io::Error,
    },
    /// Connected server UID disagreed with trusted configuration.
    #[error("broker peer UID {actual} does not match expected UID {expected}")]
    ServerIdentity {
        /// Expected UID.
        expected: u32,
        /// Actual peer UID.
        actual: u32,
    },
    /// Bounded framing failed.
    #[error("broker protocol failed")]
    Protocol(#[source] ProtocolError),
    /// Broker returned a stable public infrastructure failure.
    #[error("broker returned {code}: {message}")]
    Remote {
        /// Stable remote code.
        code: String,
        /// Bounded public message.
        message: String,
    },
    /// Response operation did not match the request.
    #[error("broker returned an unexpected response variant")]
    UnexpectedResponse,
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(PROTOCOL_VERSION)
    }
}

#[cfg(test)]
mod tests;
