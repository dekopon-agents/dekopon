//! Wire requests carry no trusted identity; the server derives authenticated context from OS peer
//! credentials before dispatching them.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::{fmt, io, time::Duration};

#[cfg(unix)]
use std::{
    env,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
};

pub use dekopon_capability::{InvocationOutcome, InvocationResult};
pub use dekopon_core::ProviderFailureDetail;

mod conversation;
#[cfg(unix)]
mod descriptor;
#[cfg(unix)]
pub use descriptor::DescriptorStream;
#[cfg(unix)]
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};

pub use conversation::{
    Conversation, ConversationKind, ConversationKindMatch, ConversationMatch,
    ConversationMatchProblem,
};
use conversation::{
    bounded_scope_part, canonical_meta_decimal, canonical_positive_service_decimal,
    canonical_signed_decimal, canonical_slack_timestamp, canonical_unsigned_decimal,
};
use dekopon_core::{
    AgentId, CapabilityId, ExternalSubject, InvocationId, ProviderId, SecretUseProposal, TraceId,
    TraceIdError, TransportId,
};
use dekopon_provider_sdk::ProviderCapability;
pub use dekopon_provider_sdk::{CommandRunOutcome, ComponentFailure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    time::timeout,
};

#[cfg(unix)]
use tokio::net::UnixStream;

pub const PROTOCOL_VERSION: &str = "dekopon.dev/broker/v1alpha2";
pub const DEFAULT_MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const HARD_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);
pub const ERROR_UNAUTHENTICATED: &str = "unauthenticated";
pub const ERROR_INVALID_REQUEST: &str = "invalid-request";
/// Nothing executed for this failure, so the same work may safely be resubmitted under a fresh
/// invocation identifier.
pub const ERROR_BROKER_UNAVAILABLE: &str = "broker-unavailable";

/// The provider's own failure text is deliberately opaque to the caller; an operator correlates the
/// code with the audit event naming the word.
pub const ERROR_PROVIDER: &str = "provider-error";
/// The external effect may already have happened, so the request must never be resubmitted; the
/// audit log is the only record of it.
pub const ERROR_OUTCOME_UNAUDITED: &str = "outcome-unaudited";
pub const ERROR_STORAGE_QUOTA: &str = "storage-quota";
pub const ERROR_STORAGE_BUSY: &str = "storage-busy";
pub const ERROR_STORAGE_TIMEOUT: &str = "storage-timeout";
pub const ERROR_STORAGE_CORRUPT: &str = "storage-corrupt";
pub const ERROR_STORAGE_IO: &str = "storage-io";

/// Nothing executed, but a full audit log cannot be fixed by a new identifier, so clients must not
/// retry automatically.
pub const ERROR_CAPACITY_EXHAUSTED: &str = "capacity-exhausted";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProtocolVersion {
    #[serde(rename = "dekopon.dev/broker/v1alpha2")]
    V1Alpha2,
}

/// This field is untrusted and reaches only telemetry and audit correlation; it is never an
/// authorization or routing input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TraceParent {
    trace: TraceId,
    parent_id: [u8; 8],
    flags: u8,
}

impl TraceParent {
    pub const fn new(
        trace_id: [u8; 16],
        parent_id: [u8; 8],
        flags: u8,
    ) -> Result<Self, TraceParentError> {
        let Ok(trace) = TraceId::new(trace_id) else {
            return Err(TraceParentError::ZeroTraceId);
        };
        if u64::from_be_bytes(parent_id) == 0 {
            return Err(TraceParentError::ZeroParentId);
        }
        Ok(Self {
            trace,
            parent_id,
            flags,
        })
    }

    #[must_use]
    pub const fn trace(&self) -> TraceId {
        self.trace
    }

    #[must_use]
    pub const fn trace_id(&self) -> [u8; 16] {
        self.trace.to_bytes()
    }

    #[must_use]
    pub const fn parent_id(&self) -> [u8; 8] {
        self.parent_id
    }

    #[must_use]
    pub const fn flags(&self) -> u8 {
        self.flags
    }
}

impl fmt::Display for TraceParent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "00-{}-", self.trace)?;
        for byte in self.parent_id {
            write!(formatter, "{byte:02x}")?;
        }
        write!(formatter, "-{:02x}", self.flags)
    }
}

impl std::str::FromStr for TraceParent {
    type Err = TraceParentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut fields = value.split('-');
        let (Some(version), Some(trace), Some(parent), Some(flags), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(TraceParentError::Malformed);
        };
        if version != "00" {
            return Err(TraceParentError::UnsupportedVersion {
                version: version.to_owned(),
            });
        }
        let trace = trace.parse::<TraceId>().map_err(|error| match error {
            TraceIdError::Zero => TraceParentError::ZeroTraceId,
            TraceIdError::Malformed => TraceParentError::Malformed,
        })?;
        let mut parent_id = [0_u8; 8];
        decode_hex(parent, &mut parent_id)?;
        let mut flag_byte = [0_u8; 1];
        decode_hex(flags, &mut flag_byte)?;
        Self::new(trace.to_bytes(), parent_id, flag_byte[0])
    }
}

#[allow(
    clippy::map_err_ignore,
    reason = "the guards below already proved exact width and all-lowercase ASCII hex, so the \
              digit-pair ParseIntError is unreachable"
)]
fn decode_hex(text: &str, output: &mut [u8]) -> Result<(), TraceParentError> {
    if text.len() != output.len() * 2 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TraceParentError::Malformed);
    }
    // Uppercase hex is rejected: W3C specifies lowercase, and accepting both would let one logical
    // context serialize two ways.
    if text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(TraceParentError::Malformed);
    }
    for (index, slot) in output.iter_mut().enumerate() {
        let start = index * 2;
        *slot = u8::from_str_radix(&text[start..start + 2], 16)
            .map_err(|_| TraceParentError::Malformed)?;
    }
    Ok(())
}

impl Serialize for TraceParent {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TraceParent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TraceParentError {
    #[error("traceparent must be `00-<32 hex>-<16 hex>-<2 hex>`")]
    Malformed,
    #[error("unsupported traceparent version {version:?}; only `00` is accepted")]
    UnsupportedVersion { version: String },
    #[error("traceparent trace identifier must not be all zeroes")]
    ZeroTraceId,
    #[error("traceparent parent identifier must not be all zeroes")]
    ZeroParentId,
}

/// Actor and principal are deliberately absent here; the server derives them from transport
/// identity, not client input.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InvocationRequest {
    pub id: InvocationId,
    pub capability: CapabilityId,
    /// This field is untrusted and mandatory; a malformed value fails decoding rather than
    /// defaulting, since an invalid trace is worse than refusing the frame.
    pub trace_parent: TraceParent,
    /// This field is proposal data only, never a credential or bearer grant, and providers never
    /// receive it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_use: Option<SecretUseProposal>,
    pub input: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatTransportKind {
    Slack,
    Discord,
    Telegram,
    Whatsapp,
    Local,
}

impl fmt::Display for ChatTransportKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Telegram => "telegram",
            Self::Whatsapp => "whatsapp",
            Self::Local => "local",
        })
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ChatScopeClaim {
    pub transport: TransportId,
    pub kind: ChatTransportKind,
    pub conversation: Conversation,
}

impl fmt::Debug for ChatScopeClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ChatScopeClaim([REDACTED])")
    }
}

impl ChatScopeClaim {
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.conversation.is_bounded()
    }
}

fn deserialize_scope_part<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_bounded_string::<D, 256>(deserializer)?;
    bounded_scope_part(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("chat scope part is not canonical"))
}

fn deserialize_bounded_string<'de, D, const MAXIMUM: usize>(
    deserializer: D,
) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor<const MAXIMUM: usize>;

    impl<'de, const MAXIMUM: usize> serde::de::Visitor<'de> for Visitor<MAXIMUM> {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a string no longer than {MAXIMUM} bytes")
        }

        fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            self.visit_str(value)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if value.len() > MAXIMUM {
                return Err(E::invalid_length(value.len(), &self));
            }
            Ok(value.to_owned())
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if value.len() > MAXIMUM {
                return Err(E::invalid_length(value.len(), &self));
            }
            Ok(value)
        }
    }

    deserializer.deserialize_string(Visitor::<MAXIMUM>)
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Attestation {
    pub subject: ExternalSubject,
    pub agent: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ChatScopeClaim>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<InvocationId>,
}

impl fmt::Debug for Attestation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Attestation([REDACTED])")
    }
}

impl Attestation {
    #[must_use]
    pub const fn for_subject(subject: ExternalSubject, agent: AgentId) -> Self {
        Self {
            subject,
            agent,
            scope: None,
            invocation: None,
        }
    }

    #[must_use]
    pub const fn for_chat(subject: ExternalSubject, agent: AgentId, scope: ChatScopeClaim) -> Self {
        Self {
            subject,
            agent,
            scope: Some(scope),
            invocation: None,
        }
    }

    #[must_use]
    pub fn bound_to(&self, invocation: InvocationId) -> Self {
        Self {
            invocation: Some(invocation),
            ..self.clone()
        }
    }

    /// This checks structure only; it consults no grant and decides nothing about authority.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.scope.as_ref().is_none_or(ChatScopeClaim::is_bounded)
    }

    #[must_use]
    pub fn binds(&self, invocation: &InvocationId) -> bool {
        self.invocation.as_ref() == Some(invocation)
    }
}

/// The tagged shape stops one transport's identifier from being replayed as another's; scope is
/// checked against the attested chat scope first.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DeliveryIdentity {
    Slack {
        #[serde(deserialize_with = "deserialize_scope_part")]
        channel: String,
        #[serde(deserialize_with = "deserialize_scope_part")]
        timestamp: String,
    },
    Discord {
        #[serde(deserialize_with = "deserialize_scope_part")]
        channel: String,
        #[serde(deserialize_with = "deserialize_scope_part")]
        message: String,
    },
    Telegram {
        #[serde(deserialize_with = "deserialize_signed_service_decimal")]
        chat: String,
        #[serde(
            default,
            deserialize_with = "deserialize_optional_positive_service_decimal"
        )]
        topic: Option<String>,
        #[serde(deserialize_with = "deserialize_positive_service_decimal")]
        message: String,
    },
    Whatsapp {
        #[serde(deserialize_with = "deserialize_meta_decimal")]
        waba: String,
        #[serde(deserialize_with = "deserialize_meta_decimal")]
        phone_number: String,
        #[serde(deserialize_with = "deserialize_whatsapp_message_id")]
        message: String,
    },
    Local {
        transport: TransportId,
        #[serde(deserialize_with = "deserialize_scope_part")]
        conversation: String,
        #[serde(deserialize_with = "deserialize_scope_part")]
        boot_nonce: String,
        connection: u64,
        sequence: u64,
    },
}

impl fmt::Debug for DeliveryIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeliveryIdentity([REDACTED])")
    }
}

impl DeliveryIdentity {
    #[must_use]
    pub fn is_canonical_for(&self, scope: &ChatScopeClaim) -> bool {
        let conversation = &scope.conversation;
        match (self, scope.kind) {
            (Self::Slack { channel, timestamp }, ChatTransportKind::Slack) => {
                channel == &conversation.id && canonical_slack_timestamp(timestamp)
            }
            (Self::Discord { channel, message }, ChatTransportKind::Discord) => {
                // A Discord thread is itself the channel its messages post in, so delivery names
                // the thread while the conversation's id names the parent.
                channel == conversation.api_channel(ChatTransportKind::Discord)
                    && canonical_unsigned_decimal(channel)
                    && canonical_unsigned_decimal(message)
            }
            (
                Self::Telegram {
                    chat,
                    topic,
                    message,
                },
                ChatTransportKind::Telegram,
            ) => {
                chat == &conversation.id
                    && canonical_signed_decimal(chat)
                    && topic.as_deref() == conversation.thread.as_deref()
                    && topic
                        .as_deref()
                        .is_none_or(canonical_positive_service_decimal)
                    && canonical_positive_service_decimal(message)
            }
            (
                Self::Whatsapp {
                    waba,
                    phone_number,
                    message,
                },
                ChatTransportKind::Whatsapp,
            ) => {
                conversation.container.as_deref() == Some(&format!("{waba}:{phone_number}"))
                    && canonical_meta_decimal(waba)
                    && canonical_meta_decimal(phone_number)
                    && canonical_whatsapp_message_id(message)
            }
            (
                Self::Local {
                    transport,
                    conversation: named,
                    boot_nonce,
                    connection,
                    sequence,
                },
                ChatTransportKind::Local,
            ) => {
                transport == &scope.transport
                    && named == &conversation.key()
                    && boot_nonce.len() == 32
                    && boot_nonce
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    && *connection > 0
                    && *sequence > 0
            }
            _ => false,
        }
    }
}

fn deserialize_whatsapp_message_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_bounded_string::<D, 256>(deserializer)?;
    canonical_whatsapp_message_id(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("WhatsApp message ID is not canonical"))
}

fn canonical_whatsapp_message_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn deserialize_meta_decimal<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_bounded_string::<D, 64>(deserializer)?;
    canonical_meta_decimal(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("identifier is not a canonical Meta decimal"))
}

fn deserialize_positive_service_decimal<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_bounded_string::<D, 256>(deserializer)?;
    canonical_positive_service_decimal(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("identifier is outside the positive service range"))
}

fn deserialize_signed_service_decimal<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_bounded_string::<D, 256>(deserializer)?;
    canonical_signed_decimal(&value)
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("identifier is outside the signed service range"))
}

fn deserialize_optional_positive_service_decimal<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalServiceDecimal;

    impl<'de> serde::de::Visitor<'de> for OptionalServiceDecimal {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("null or a canonical positive signed-service identifier")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<Inner>(self, deserializer: Inner) -> Result<Self::Value, Inner::Error>
        where
            Inner: serde::Deserializer<'de>,
        {
            deserialize_positive_service_decimal(deserializer).map(Some)
        }
    }

    deserializer.deserialize_option(OptionalServiceDecimal)
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeliveredTurnRequest {
    pub id: InvocationId,
    pub trace_parent: TraceParent,
    pub delivery: DeliveryIdentity,
    #[serde(deserialize_with = "deserialize_turn_text")]
    pub user: String,
    #[serde(deserialize_with = "deserialize_turn_text")]
    pub assistant: String,
}

impl fmt::Debug for DeliveredTurnRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeliveredTurnRequest([REDACTED])")
    }
}

impl DeliveredTurnRequest {
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.user
            .len()
            .checked_add(self.assistant.len())
            .is_some_and(|bytes| bytes <= 64 * 1024)
    }
}

fn deserialize_turn_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_string::<D, { 64 * 1024 }>(deserializer)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ChatMemorySurface {
    pub max_lookback_turns: u32,
    pub prompt_note: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AvailableCapability {
    pub provider: ProviderId,
    pub capability: ProviderCapability,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RequestEnvelope {
    pub api_version: ProtocolVersion,
    pub request: BrokerRequest,
}

impl RequestEnvelope {
    #[must_use]
    pub const fn capabilities(attestation: Option<Attestation>) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha2,
            request: BrokerRequest::Capabilities { attestation },
        }
    }

    #[must_use]
    pub const fn run_command(
        attestation: Option<Attestation>,
        word: String,
        argv: Vec<String>,
        stdin: Option<String>,
        trace_parent: TraceParent,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha2,
            request: BrokerRequest::RunCommand {
                attestation,
                trace_parent,
                word,
                argv,
                stdin,
            },
        }
    }

    #[must_use]
    pub const fn invoke(
        attestation: Option<Attestation>,
        invocation: InvocationRequest,
        assets: Vec<AssetRow>,
        sends_remaining: u8,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha2,
            request: BrokerRequest::Invoke {
                attestation,
                invocation,
                assets,
                sends_remaining,
            },
        }
    }

    #[must_use]
    pub const fn record_delivered_turn(
        attestation: Attestation,
        turn: DeliveredTurnRequest,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha2,
            request: BrokerRequest::RecordDeliveredTurn { attestation, turn },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "operation", deny_unknown_fields, rename_all = "camelCase")]
pub enum BrokerRequest {
    Capabilities {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
    },
    RunCommand {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
        #[serde(rename = "traceParent")]
        trace_parent: TraceParent,
        word: String,
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stdin: Option<String>,
    },
    Invoke {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attestation: Option<Attestation>,
        invocation: InvocationRequest,
        #[serde(default)]
        assets: Vec<AssetRow>,
        #[serde(default, rename = "sendsRemaining")]
        sends_remaining: u8,
    },
    RecordDeliveredTurn {
        attestation: Attestation,
        turn: DeliveredTurnRequest,
    },
}

impl BrokerRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if matches!(self, Self::Invoke { assets, .. } if assets.len() > MAX_ASSET_ROWS) {
            return Err(ProtocolError::TooManyAssetRows);
        }
        Ok(())
    }
}

pub const MAX_ASSET_ROWS: usize = 32;
pub const MAX_DESCRIPTORS_PER_FRAME: usize = 5;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AssetRow {
    pub id: u64,
    pub content_type: String,
    pub encoding: AssetEncoding,
    pub bytes: Option<u64>,
    pub origin: String,
    pub sent: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AssetEncoding {
    Identity,
    Base64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NewAsset {
    pub descriptor: u32,
    pub content_type: String,
    pub encoding: AssetEncoding,
    pub bytes: u64,
    pub sha256: String,
}

#[cfg(unix)]
#[derive(Debug, Default)]
pub struct InvokeAssets {
    pub rows: Vec<AssetRow>,
    pub sends_remaining: u8,
    pub descriptors: Vec<OwnedFd>,
}

#[cfg(unix)]
#[derive(Debug)]
pub struct AssetInvocationOutcome {
    pub result: InvocationResult,
    pub attached: Vec<NewAsset>,
    pub removed: Vec<u64>,
    pub sent: Vec<u64>,
    pub descriptors: Vec<OwnedFd>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ResponseEnvelope {
    pub api_version: ProtocolVersion,
    pub response: BrokerResponse,
}

impl ResponseEnvelope {
    #[must_use]
    pub const fn capabilities(
        capabilities: Vec<AvailableCapability>,
        command_words: Vec<String>,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha2,
            response: BrokerResponse::Capabilities {
                capabilities,
                command_words,
                chat_memory: None,
            },
        }
    }

    #[must_use]
    pub const fn chat_capabilities(
        capabilities: Vec<AvailableCapability>,
        command_words: Vec<String>,
        chat_memory: Option<ChatMemorySurface>,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha2,
            response: BrokerResponse::Capabilities {
                capabilities,
                command_words,
                chat_memory,
            },
        }
    }

    #[must_use]
    pub const fn command_run(result: CommandRunOutcome) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha2,
            response: BrokerResponse::CommandRun { result },
        }
    }

    #[must_use]
    pub const fn invocation(
        result: InvocationResult,
        attached: Vec<NewAsset>,
        removed: Vec<u64>,
        sent: Vec<u64>,
    ) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha2,
            response: BrokerResponse::Invocation {
                result,
                attached,
                removed,
                sent,
            },
        }
    }

    #[must_use]
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            api_version: ProtocolVersion::V1Alpha2,
            response: BrokerResponse::Error {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", deny_unknown_fields, rename_all = "camelCase")]
pub enum BrokerResponse {
    Capabilities {
        capabilities: Vec<AvailableCapability>,
        #[serde(default)]
        command_words: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat_memory: Option<ChatMemorySurface>,
    },
    Invocation {
        result: InvocationResult,
        #[serde(default)]
        attached: Vec<NewAsset>,
        #[serde(default)]
        removed: Vec<u64>,
        #[serde(default)]
        sent: Vec<u64>,
    },
    CommandRun {
        result: CommandRunOutcome,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameLimits {
    pub max_frame_bytes: usize,
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

const FRAME_PREFIX_BYTES: usize = 4;
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Allocation follows bytes that actually arrive rather than the peer's claimed length, so a
/// stalling peer holds one chunk, not a whole frame's memory.
pub async fn read_frame<R, T>(reader: &mut R, limits: FrameLimits) -> Result<T, ProtocolError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let limits = limits.validate()?;
    #[allow(
        clippy::map_err_ignore,
        reason = "tokio's Elapsed says only that io_timeout expired, which ProtocolError::Timeout \
                  already states"
    )]
    let bytes = timeout(limits.io_timeout, async {
        let mut prefix = [0_u8; FRAME_PREFIX_BYTES];
        reader.read_exact(&mut prefix).await?;
        let length = usize::try_from(u32::from_be_bytes(prefix)).unwrap_or(usize::MAX);
        if length == 0 {
            return Err(ReadFrameError::Empty);
        }
        if length > limits.max_frame_bytes {
            return Err(ReadFrameError::TooLarge { length });
        }
        read_payload(reader, length).await
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

/// Growth reserves one chunk at a time and stops exactly at length, avoiding the overshoot a
/// doubling strategy causes on large frames.
async fn read_payload<R>(reader: &mut R, length: usize) -> Result<Vec<u8>, ReadFrameError>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut filled = 0;
    while filled < length {
        if bytes.len() == filled {
            let target = bytes.len().saturating_add(READ_CHUNK_BYTES).min(length);
            bytes.reserve_exact(target - bytes.len());
            bytes.resize(target, 0);
        }
        let read = reader.read(&mut bytes[filled..]).await?;
        if read == 0 {
            return Err(ReadFrameError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "broker frame ended before its declared length",
            )));
        }
        filled += read;
    }
    bytes.truncate(filled);
    Ok(bytes)
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
    // Counting the payload before writing lets the frame allocate exactly once at final size and
    // refuse an oversized value before holding any of it.
    let mut counter = BoundedJsonCounter::new(limits.max_frame_bytes);
    if let Err(source) = serde_json::to_writer(&mut counter, value) {
        return Err(frame_write_failure(
            source,
            counter.exceeded,
            limits.max_frame_bytes,
        ));
    }
    let mut buffer = BoundedJsonBuffer::new(limits.max_frame_bytes, counter.length);
    if let Err(source) = serde_json::to_writer(&mut buffer, value) {
        return Err(frame_write_failure(
            source,
            buffer.exceeded,
            limits.max_frame_bytes,
        ));
    }
    let payload = buffer.payload_len();
    #[allow(
        clippy::map_err_ignore,
        reason = "TryFromIntError carries only out-of-range, and FrameTooLarge already names the \
                  length and the maximum"
    )]
    let length = u32::try_from(payload).map_err(|_| ProtocolError::FrameTooLarge {
        length: payload,
        maximum: limits.max_frame_bytes,
    })?;
    buffer.frame[..FRAME_PREFIX_BYTES].copy_from_slice(&length.to_be_bytes());
    #[allow(
        clippy::map_err_ignore,
        reason = "tokio's Elapsed says only that io_timeout expired, which ProtocolError::Timeout \
                  already states"
    )]
    timeout(limits.io_timeout, async {
        writer.write_all(&buffer.frame).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| ProtocolError::Timeout)?
    .map_err(|source| ProtocolError::Io { source })
}

fn frame_write_failure(source: serde_json::Error, exceeded: bool, maximum: usize) -> ProtocolError {
    if exceeded {
        return ProtocolError::FrameTooLarge {
            length: maximum.saturating_add(1),
            maximum,
        };
    }
    ProtocolError::Serialize { source }
}

struct BoundedJsonCounter {
    length: usize,
    maximum: usize,
    exceeded: bool,
}

impl BoundedJsonCounter {
    fn new(maximum: usize) -> Self {
        Self {
            length: 0,
            maximum,
            exceeded: false,
        }
    }
}

impl io::Write for BoundedJsonCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.length.checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON frame overflowed"));
        };
        if length > self.maximum {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON frame exceeded its limit"));
        }
        self.length = length;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BoundedJsonBuffer {
    frame: Vec<u8>,
    maximum: usize,
    exceeded: bool,
}

impl BoundedJsonBuffer {
    fn new(maximum: usize, payload: usize) -> Self {
        let mut frame = Vec::with_capacity(FRAME_PREFIX_BYTES + payload);
        frame.extend_from_slice(&[0_u8; FRAME_PREFIX_BYTES]);
        Self {
            frame,
            maximum,
            exceeded: false,
        }
    }

    fn payload_len(&self) -> usize {
        self.frame.len() - FRAME_PREFIX_BYTES
    }
}

impl io::Write for BoundedJsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.payload_len().checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON frame overflowed"));
        };
        if length > self.maximum {
            self.exceeded = true;
            return Err(io::Error::other("bounded JSON frame exceeded its limit"));
        }
        self.frame.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("broker frame descriptors were truncated")]
    DescriptorsTruncated,
    #[error("could not set broker descriptor close-on-exec flags")]
    DescriptorFlags {
        #[source]
        source: io::Error,
    },
    #[error("broker frame has too many descriptors")]
    TooManyDescriptors,
    #[error("broker frame has unexpected descriptors")]
    UnexpectedDescriptors,
    #[error("broker frame has invalid descriptor indexes")]
    DescriptorIndex,
    #[error("broker frame has too many asset rows")]
    TooManyAssetRows,
    #[error("frame maximum must be between 1 and {maximum} bytes")]
    InvalidFrameLimit { maximum: usize },
    #[error("frame I/O timeout must be greater than zero")]
    ZeroTimeout,
    #[error("broker frame I/O timed out")]
    Timeout,
    #[error("broker frame I/O failed")]
    Io {
        #[source]
        source: io::Error,
    },
    #[error("broker frame must not be empty")]
    EmptyFrame,
    #[error("broker frame is {length} bytes; maximum is {maximum}")]
    FrameTooLarge { length: usize, maximum: usize },
    #[error("could not serialize broker frame")]
    Serialize {
        #[source]
        source: serde_json::Error,
    },
    #[error("broker frame is not valid protocol JSON")]
    Deserialize {
        #[source]
        source: serde_json::Error,
    },
}

#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct BrokerClient {
    socket: PathBuf,
    expected_server_uid: u32,
    limits: FrameLimits,
}

#[cfg(unix)]
impl BrokerClient {
    pub fn new(
        socket: impl Into<PathBuf>,
        expected_server_uid: u32,
        limits: FrameLimits,
    ) -> Result<Self, ClientError> {
        Ok(Self {
            socket: socket.into(),
            expected_server_uid,
            limits: limits.validate().map_err(ClientError::Limits)?,
        })
    }

    /// The three refusal causes are deliberately indistinguishable, so a refused caller cannot
    /// learn whether the subject exists.
    pub async fn session_surface(
        &self,
        attestation: Option<Attestation>,
    ) -> Result<
        (
            Vec<AvailableCapability>,
            Vec<String>,
            Option<ChatMemorySurface>,
        ),
        ClientError,
    > {
        match self
            .exchange(RequestEnvelope::capabilities(attestation))
            .await?
        {
            BrokerResponse::Capabilities {
                capabilities,
                command_words,
                chat_memory,
            } => Ok((capabilities, command_words, chat_memory)),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::CommandRun { .. } | BrokerResponse::Invocation { .. } => {
                Err(ClientError::UnexpectedResponse)
            }
        }
    }

    pub async fn capabilities(&self) -> Result<Vec<AvailableCapability>, ClientError> {
        Ok(self.session_surface(None).await?.0)
    }

    /// A refused attestation must answer exactly as an unknown word would, since naming the word
    /// would disclose the surface withheld.
    pub async fn run_command(
        &self,
        attestation: Option<Attestation>,
        word: String,
        argv: Vec<String>,
        stdin: Option<String>,
        trace_parent: TraceParent,
    ) -> Result<CommandRunOutcome, ClientError> {
        match self
            .exchange(RequestEnvelope::run_command(
                attestation,
                word,
                argv,
                stdin,
                trace_parent,
            ))
            .await?
        {
            BrokerResponse::CommandRun { result } => Ok(result),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::Capabilities { .. } | BrokerResponse::Invocation { .. } => {
                Err(ClientError::UnexpectedResponse)
            }
        }
    }

    pub async fn invoke(
        &self,
        attestation: Option<Attestation>,
        request: InvocationRequest,
        assets: InvokeAssets,
    ) -> Result<AssetInvocationOutcome, ClientError> {
        let attestation = attestation.map(|claim| claim.bound_to(request.id.clone()));
        let descriptors: Vec<_> = assets.descriptors.iter().map(|fd| fd.as_fd()).collect();
        let (response, descriptors) = self
            .exchange_assets(
                RequestEnvelope::invoke(attestation, request, assets.rows, assets.sends_remaining),
                &descriptors,
            )
            .await?;
        match response {
            BrokerResponse::Invocation {
                result,
                attached,
                removed,
                sent,
            } => Ok(AssetInvocationOutcome {
                result,
                attached,
                removed,
                sent,
                descriptors,
            }),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::Capabilities { .. } | BrokerResponse::CommandRun { .. } => {
                Err(ClientError::UnexpectedResponse)
            }
        }
    }

    pub async fn record_delivered_turn(
        &self,
        attestation: Attestation,
        turn: DeliveredTurnRequest,
    ) -> Result<InvocationResult, ClientError> {
        let attestation = attestation.bound_to(turn.id.clone());
        match self
            .exchange(RequestEnvelope::record_delivered_turn(attestation, turn))
            .await?
        {
            BrokerResponse::Invocation { result, .. } => Ok(result),
            BrokerResponse::Error { code, message } => Err(ClientError::Remote { code, message }),
            BrokerResponse::Capabilities { .. } | BrokerResponse::CommandRun { .. } => {
                Err(ClientError::UnexpectedResponse)
            }
        }
    }

    async fn exchange(&self, request: RequestEnvelope) -> Result<BrokerResponse, ClientError> {
        Ok(self.exchange_assets(request, &[]).await?.0)
    }

    async fn exchange_assets(
        &self,
        request: RequestEnvelope,
        descriptors: &[BorrowedFd<'_>],
    ) -> Result<(BrokerResponse, Vec<OwnedFd>), ClientError> {
        request
            .request
            .validate()
            .map_err(|source| ClientError::Protocol {
                phase: ExchangePhase::Request,
                source,
            })?;
        validate_socket_path(&self.socket, self.expected_server_uid).await?;
        #[allow(
            clippy::map_err_ignore,
            reason = "tokio's Elapsed says only that io_timeout expired, which \
                      ClientError::ConnectTimeout already states"
        )]
        let stream = timeout(self.limits.io_timeout, UnixStream::connect(&self.socket))
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
        let mut stream = DescriptorStream::new(stream);
        stream
            .write_frame(&request, descriptors, self.limits)
            .await
            .map_err(|source| ClientError::Protocol {
                phase: ExchangePhase::Request,
                source,
            })?;
        let (response, descriptors) = stream
            .read_frame::<ResponseEnvelope>(self.limits)
            .await
            .map_err(|source| ClientError::Protocol {
                phase: ExchangePhase::Response,
                source,
            })?;
        validate_response_descriptors(&response.response, descriptors.len()).map_err(|source| {
            ClientError::Protocol {
                phase: ExchangePhase::Response,
                source,
            }
        })?;
        Ok((response.response, descriptors))
    }
}

#[cfg(unix)]
fn validate_response_descriptors(
    response: &BrokerResponse,
    count: usize,
) -> Result<(), ProtocolError> {
    match response {
        BrokerResponse::Invocation { attached, .. } => {
            if attached.len() != count
                || attached
                    .iter()
                    .enumerate()
                    .any(|(index, asset)| usize::try_from(asset.descriptor) != Ok(index))
            {
                return Err(ProtocolError::DescriptorIndex);
            }
        }
        _ if count != 0 => return Err(ProtocolError::UnexpectedDescriptors),
        _ => {}
    }
    Ok(())
}

/// The parent directory is inspected regardless of the socket's own mode, so this client trusts
/// exactly the sockets the broker would bind.
#[cfg(unix)]
async fn validate_socket_path(path: &Path, expected_uid: u32) -> Result<(), ClientError> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|source| ClientError::SocketMetadata { source })?;
    let parent = path.parent().ok_or(ClientError::UnsafeSocket)?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let parent = tokio::fs::symlink_metadata(parent)
        .await
        .map_err(|source| ClientError::SocketMetadata { source })?;
    if !secure_socket_parent(&parent, expected_uid)
        || !secure_socket(&metadata, expected_uid, &parent)
    {
        return Err(ClientError::UnsafeSocket);
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("could not inspect broker socket")]
    SocketMetadata {
        #[source]
        source: io::Error,
    },
    #[error("broker socket or parent has unsafe permissions or ownership")]
    UnsafeSocket,
    #[error("broker connection timed out")]
    ConnectTimeout,
    #[error("could not connect to broker socket")]
    Connect {
        #[source]
        source: io::Error,
    },
    #[error("could not authenticate broker peer credentials")]
    PeerCredentials {
        #[source]
        source: io::Error,
    },
    #[error("broker peer UID {actual} does not match expected UID {expected}")]
    ServerIdentity { expected: u32, actual: u32 },
    #[error("broker client limits are invalid: {0}")]
    Limits(#[source] ProtocolError),
    #[error("broker {phase} framing failed: {source}")]
    Protocol {
        phase: ExchangePhase,
        #[source]
        source: ProtocolError,
    },
    #[error("broker returned {code}: {message}")]
    Remote { code: String, message: String },
    #[error("broker returned an unexpected response variant")]
    UnexpectedResponse,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExchangePhase {
    Request,
    Response,
}

#[cfg(unix)]
impl fmt::Display for ExchangePhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Request => "request",
            Self::Response => "response",
        })
    }
}

#[cfg(unix)]
impl ClientError {
    #[must_use]
    pub fn may_have_executed(&self) -> bool {
        match self {
            Self::Protocol { phase, .. } => *phase == ExchangePhase::Response,
            Self::Remote { code, .. } => code == ERROR_OUTCOME_UNAUDITED,
            Self::UnexpectedResponse => true,
            _ => false,
        }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::V1Alpha2 => PROTOCOL_VERSION,
        })
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerSocketTier {
    Explicit,
    Environment,
    XdgRuntimeDir,
    Home,
}

#[cfg(unix)]
impl BrokerSocketTier {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Environment => "environment",
            Self::XdgRuntimeDir => "xdg-runtime-dir",
            Self::Home => "home",
        }
    }
}

#[cfg(unix)]
impl fmt::Display for BrokerSocketTier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedBrokerSocket {
    path: PathBuf,
    tier: BrokerSocketTier,
}

#[cfg(unix)]
impl ResolvedBrokerSocket {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn into_path(self) -> PathBuf {
        self.path
    }

    #[must_use]
    pub const fn tier(&self) -> BrokerSocketTier {
        self.tier
    }
}

/// The socket directory must be server-owned and never group-writable or reachable by others; no
/// credential path uses this rule.
#[cfg(unix)]
#[must_use]
pub fn secure_socket_parent(parent: &std::fs::Metadata, expected_uid: u32) -> bool {
    let mode = parent.permissions().mode();
    parent.file_type().is_dir()
        && parent.uid() == expected_uid
        && mode & 0o027 == 0
        && matches!(mode & 0o070, 0 | 0o010 | 0o050)
}

/// A 0600 socket is owner-only; a 0660 socket is trustworthy only inside a group-traversable parent
/// whose group it carries.
#[cfg(unix)]
#[must_use]
pub fn secure_socket(
    socket: &std::fs::Metadata,
    expected_uid: u32,
    parent: &std::fs::Metadata,
) -> bool {
    let mode = socket.permissions().mode() & 0o7777;
    socket.file_type().is_socket()
        && socket.uid() == expected_uid
        && socket.nlink() == 1
        && (mode == 0o600
            || (mode == 0o660
                && parent.permissions().mode() & 0o010 != 0
                && socket.gid() == parent.gid()))
}

/// A private parent keeps the socket owner-only, which is why a broker under one refuses any peer
/// UID but its own.
#[cfg(unix)]
#[must_use]
pub fn ipc_socket_mode(parent: &std::fs::Metadata) -> u32 {
    if parent.permissions().mode() & 0o010 == 0 {
        0o600
    } else {
        0o660
    }
}

#[cfg(unix)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BrokerSocketDiscovery {
    explicit: Option<PathBuf>,
    environment: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
    home: Option<PathBuf>,
}

#[cfg(unix)]
impl BrokerSocketDiscovery {
    #[must_use]
    pub fn from_process(explicit: Option<PathBuf>) -> Self {
        Self {
            explicit,
            environment: env::var_os("DEKOPON_BROKER_SOCKET")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            xdg_runtime_dir: env::var_os("XDG_RUNTIME_DIR")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            home: env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        }
    }

    #[must_use]
    pub const fn new(
        explicit: Option<PathBuf>,
        environment: Option<PathBuf>,
        xdg_runtime_dir: Option<PathBuf>,
        home: Option<PathBuf>,
    ) -> Self {
        Self {
            explicit,
            environment,
            xdg_runtime_dir,
            home,
        }
    }

    #[must_use]
    pub fn resolve(&self) -> Option<ResolvedBrokerSocket> {
        if let Some(path) = &self.explicit {
            return Some(ResolvedBrokerSocket {
                path: path.clone(),
                tier: BrokerSocketTier::Explicit,
            });
        }
        if let Some(path) = &self.environment {
            return Some(ResolvedBrokerSocket {
                path: path.clone(),
                tier: BrokerSocketTier::Environment,
            });
        }
        if let Some(root) = &self.xdg_runtime_dir {
            return Some(ResolvedBrokerSocket {
                path: root.join("dekopon/broker.sock"),
                tier: BrokerSocketTier::XdgRuntimeDir,
            });
        }
        if let Some(home) = &self.home {
            return Some(ResolvedBrokerSocket {
                path: home.join(".local/run/dekopon/broker.sock"),
                tier: BrokerSocketTier::Home,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests;
