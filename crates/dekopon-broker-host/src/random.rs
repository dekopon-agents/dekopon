use crate::{StoreState, bindings::dekopon::random::source};

const MAX_RANDOM_BYTES: u32 = 4096;

pub(crate) enum RandomState {
    Refused { attempted: bool },
    Granted { failure: Option<&'static str> },
}

impl RandomState {
    pub(crate) const fn describe() -> Self {
        Self::Refused { attempted: false }
    }

    pub(crate) const fn invoke() -> Self {
        Self::Granted { failure: None }
    }

    pub(crate) const fn attempted(&self) -> bool {
        matches!(self, Self::Refused { attempted: true })
    }

    pub(crate) const fn failure(&self) -> Option<&'static str> {
        match self {
            Self::Granted { failure } => *failure,
            Self::Refused { .. } => None,
        }
    }

    fn read(
        &mut self,
        length: u32,
        fill: impl FnOnce(&mut [u8]) -> Result<(), getrandom::Error>,
    ) -> wasmtime::Result<Vec<u8>> {
        let failure = match self {
            Self::Refused { attempted } => {
                *attempted = true;
                tracing::info!(
                    event = "provider_random_read",
                    length,
                    status = "refused-phase"
                );
                return Err(wasmtime::Error::msg("random bytes outside invoke"));
            }
            Self::Granted { failure } => failure,
        };
        if length > MAX_RANDOM_BYTES {
            failure.get_or_insert("random-too-large");
            tracing::info!(
                event = "provider_random_read",
                length,
                status = "refused-size"
            );
            return Err(wasmtime::Error::msg("random request exceeds host limit"));
        }
        let mut bytes = vec![0; length as usize];
        if length == 0 {
            tracing::info!(event = "provider_random_read", length, status = "succeeded");
            return Ok(bytes);
        }
        if let Err(source) = fill(&mut bytes) {
            failure.get_or_insert("random-entropy");
            tracing::error!(
                event = "provider_random_read",
                length,
                status = "entropy-failed",
                ?source
            );
            return Err(wasmtime::Error::msg("OS entropy unavailable"));
        }
        tracing::info!(event = "provider_random_read", length, status = "succeeded");
        Ok(bytes)
    }
}

impl source::Host for StoreState {
    async fn get_random_bytes(&mut self, length: u32) -> wasmtime::Result<Vec<u8>> {
        self.random.read(length, getrandom::fill)
    }
}
