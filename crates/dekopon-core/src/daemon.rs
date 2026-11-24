use thiserror::Error;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[cfg(unix)]
    #[error(transparent)]
    Replace(#[from] crate::private_file::PrivateFileError),
    #[cfg(unix)]
    #[error(transparent)]
    Read(#[from] crate::FileHygieneError),
    #[error("record is {length} bytes; the maximum is {maximum}")]
    TooLarge { length: u64, maximum: usize },
    #[error("could not decode the record")]
    Decode {
        #[source]
        source: serde_json::Error,
    },
    #[error("the supervisor already holds its maximum number of children")]
    TooManyChildren,
}

impl DaemonError {
    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            #[cfg(unix)]
            Self::Replace(_) => "replace",
            #[cfg(unix)]
            Self::Read(error) => error.category(),
            Self::TooLarge { .. } => "too-large",
            Self::Decode { .. } => "decode",
            Self::TooManyChildren => "too-many-children",
        }
    }
}
