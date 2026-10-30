use crate::asset::AssetSourceRef;
use dekopon_agent::memory::Item;
use dekopon_core::AssetNumber;
use dekopon_core::asset::AssetKind;
use dekopon_model::model::ModelKey;
use serde::{Deserialize, Serialize};
use std::{
    io,
    num::{NonZeroU32, NonZeroU64},
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct RecordId(pub NonZeroU64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TurnId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct GrantDigest(pub [u8; 8]);

impl GrantDigest {
    pub(crate) fn of(granted: &[String]) -> Self {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        for capability in granted {
            digest.update((capability.len() as u64).to_be_bytes());
            digest.update(capability.as_bytes());
        }
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&digest.finalize()[..8]);
        Self(bytes)
    }

    pub(crate) fn hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

pub(crate) struct TurnRecord {
    pub id: RecordId,
    pub turn: TurnId,
    pub at_ms: u64,
    pub grant: GrantDigest,
    pub model: ModelKey,
    pub original: Vec<Item>,
    pub summaries: Vec<Slot>,
    pub plan: Option<SummaryPlan>,
    pub assets: Vec<AssetRow>,
    pub next_asset_id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct AssetRow {
    pub id: u64,
    pub at_ms: u64,
    pub name: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<AssetSourceRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SummaryPlan {
    pub model: String,
    pub parts: Vec<NonZeroU32>,
    pub max_input_bytes: NonZeroU32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum Slot {
    Queued,
    Running,
    Generated(String),
    Error(SummaryError),
}

impl Slot {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Generated(_) => "generated",
            Self::Error(_) => "error",
        }
    }

    pub(crate) const fn is_pending(&self) -> bool {
        match self {
            Self::Queued | Self::Running => true,
            Self::Generated(_) | Self::Error(_) => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SummaryError {
    pub kind: SummaryFailure,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum SummaryFailure {
    Timeout,
    Refused,
    ModelFailed,
    Empty,
    Overflow,
    Panicked,
    Io {
        #[serde(with = "io_kind")]
        kind: io::ErrorKind,
    },
    Unconfigured,
}

impl SummaryFailure {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Refused => "refused",
            Self::ModelFailed => "model-failed",
            Self::Empty => "empty",
            Self::Overflow => "overflow",
            Self::Panicked => "panicked",
            Self::Io { .. } => "io",
            Self::Unconfigured => "unconfigured",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArrivedAsset {
    pub item: u32,
    pub number: AssetNumber,
    pub kind: AssetKind,
    pub bytes: Option<u64>,
    pub summaries: Vec<Slot>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Subject {
    #[default]
    Record,
    Asset(AssetNumber),
}

impl Subject {
    pub(crate) const fn is_record(&self) -> bool {
        match self {
            Self::Record => true,
            Self::Asset(_) => false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SlotLine {
    pub at_ms: u64,
    pub record: RecordId,
    pub part: std::num::NonZeroU8,
    pub slot: Slot,
    #[serde(default, skip_serializing_if = "Subject::is_record")]
    pub subject: Subject,
}

mod io_kind {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::io::ErrorKind;

    pub(super) fn serialize<S: Serializer>(
        kind: &ErrorKind,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("{kind:?}"))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ErrorKind, D::Error> {
        let label = String::deserialize(deserializer)?;
        macro_rules! kinds {
            ($($kind:ident),+ $(,)?) => {
                match label.as_str() {
                    $(stringify!($kind) => Ok(ErrorKind::$kind),)+
                    label => {
                        (0..4096).chain(std::iter::once(i32::MAX))
                            .map(|code| std::io::Error::from_raw_os_error(code).kind())
                            .find(|kind| label == format!("{kind:?}"))
                            .ok_or_else(|| serde::de::Error::custom("unknown I/O error kind"))
                    }
                }
            };
        }
        kinds!(
            NotFound,
            PermissionDenied,
            ConnectionRefused,
            ConnectionReset,
            HostUnreachable,
            NetworkUnreachable,
            ConnectionAborted,
            NotConnected,
            AddrInUse,
            AddrNotAvailable,
            NetworkDown,
            BrokenPipe,
            AlreadyExists,
            WouldBlock,
            NotADirectory,
            IsADirectory,
            DirectoryNotEmpty,
            ReadOnlyFilesystem,
            StaleNetworkFileHandle,
            InvalidInput,
            InvalidData,
            TimedOut,
            WriteZero,
            StorageFull,
            NotSeekable,
            QuotaExceeded,
            FileTooLarge,
            ResourceBusy,
            ExecutableFileBusy,
            Deadlock,
            CrossesDevices,
            TooManyLinks,
            InvalidFilename,
            ArgumentListTooLong,
            Interrupted,
            Unsupported,
            UnexpectedEof,
            OutOfMemory,
            Other
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct PinLine {
    pub at_ms: u64,
    pub record: RecordId,
    pub pin: Option<dekopon_agent::memory_tools::Rung>,
    #[serde(default, skip_serializing_if = "Subject::is_record")]
    pub subject: Subject,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ChoiceLine {
    pub at_ms: u64,
    pub record: RecordId,
    pub choice: dekopon_agent::memory_tools::Rung,
}

#[derive(Clone, Debug)]
pub(crate) enum AssetSideLine {
    Slot(SlotLine),
    Pin(PinLine),
}

pub(crate) struct AssetState {
    pub summaries: Vec<Slot>,
    pub pin: Option<dekopon_agent::memory_tools::Rung>,
}

impl ArrivedAsset {
    pub(crate) fn join(&self, lines: &[AssetSideLine]) -> AssetState {
        let mut state = AssetState {
            summaries: self.summaries.clone(),
            pin: None,
        };
        for line in lines {
            match line {
                AssetSideLine::Slot(line) => {
                    if line.subject == Subject::Asset(self.number)
                        && let Some(slot) =
                            state.summaries.get_mut(usize::from(line.part.get() - 1))
                    {
                        *slot = line.slot.clone();
                    }
                }
                AssetSideLine::Pin(line) => {
                    if line.subject == Subject::Asset(self.number) {
                        state.pin = line.pin;
                    }
                }
            }
        }
        state
    }
}
