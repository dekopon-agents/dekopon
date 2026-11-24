use std::{
    fs::{self},
    io::{self, Read as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
};

use dekopon_core::{
    AgentId,
    private_file::{
        PrivateFileContents, TemporarySweep, replace_private_file, sweep_stale_temporaries,
    },
};
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::{Sha256, digest::Update};
use thiserror::Error;

use crate::conversation::ConversationKey;

const DOMAIN: &[u8] = b"dekopon-gatewayd-prompt-cache-key-v1";

const CONVERSATION_PREFIX: &str = "gatewayd-conversation";

const ROUTE_PREFIX: &str = "gatewayd-route";

const SECRET_FILE: &str = "prompt-cache-key.secret";

const SECRET_BYTES: usize = 32;

#[derive(Debug, Error)]
pub enum CacheKeySecretError {
    #[error("the operating system refused entropy for the prompt cache key secret")]
    Entropy(#[source] getrandom::Error),
    #[error("prompt cache key secret {} could not be written: {kind}", path.display())]
    Io { path: PathBuf, kind: io::ErrorKind },
}

#[derive(Clone, Copy)]
enum Reset {
    Missing,
    LooseMode,
    Malformed,
}

impl Reset {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::LooseMode => "loose-mode",
            Self::Malformed => "malformed",
        }
    }
}

#[derive(Clone)]
pub(crate) struct CacheKeySecret(Hmac<Sha256>);

impl CacheKeySecret {
    pub(crate) fn generate() -> Result<Self, CacheKeySecretError> {
        Ok(Self::from_bytes(fresh()?))
    }

    pub(crate) fn load_or_create(journal: &Path) -> Result<Self, CacheKeySecretError> {
        let path = journal.join(SECRET_FILE);
        let reset = match read(&path) {
            Ok(bytes) => return Ok(Self::from_bytes(bytes)),
            Err(reset) => reset,
        };
        let bytes = fresh()?;
        write(&path, &bytes).map_err(|error| CacheKeySecretError::Io {
            path,
            kind: error.kind(),
        })?;
        tracing::warn!(
            event = "gateway_cache_key_secret_reset",
            reason = reset.as_str(),
            "gateway.cache.key.secret.reset"
        );
        Ok(Self::from_bytes(bytes))
    }

    pub(crate) fn from_bytes(bytes: [u8; SECRET_BYTES]) -> Self {
        let mut block = [0_u8; 64];
        block[..SECRET_BYTES].copy_from_slice(&bytes);
        Self(Hmac::new(&block.into()))
    }

    pub(crate) fn for_conversation(&self, key: &ConversationKey) -> String {
        let mut mac = self.lane(CONVERSATION_PREFIX);
        key.absorb(&mut mac);
        finish(CONVERSATION_PREFIX, mac)
    }

    pub(crate) fn for_route(&self, agent: &AgentId, transport: &str, route_index: usize) -> String {
        let mut mac = self.lane(ROUTE_PREFIX);
        for part in [
            agent.as_str().as_bytes(),
            transport.as_bytes(),
            &(route_index as u64).to_be_bytes(),
        ] {
            absorb(&mut mac, part);
        }
        finish(ROUTE_PREFIX, mac)
    }

    fn lane(&self, prefix: &str) -> Hmac<Sha256> {
        let mut mac = self.0.clone();
        absorb(&mut mac, DOMAIN);
        absorb(&mut mac, prefix.as_bytes());
        mac
    }
}

fn fresh() -> Result<[u8; SECRET_BYTES], CacheKeySecretError> {
    let mut bytes = [0_u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(CacheKeySecretError::Entropy)?;
    Ok(bytes)
}

fn read(path: &Path) -> Result<[u8; SECRET_BYTES], Reset> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Err(Reset::Missing);
    };
    if !metadata.is_file() {
        return Err(Reset::Malformed);
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(Reset::LooseMode);
    }
    let mut bytes = Vec::with_capacity(SECRET_BYTES + 1);
    fs::File::open(path)
        .and_then(|file| file.take(SECRET_BYTES as u64 + 1).read_to_end(&mut bytes))
        .map_err(|_unreadable| Reset::Malformed)?;
    <[u8; SECRET_BYTES]>::try_from(bytes.as_slice()).map_err(|_wrong_length| Reset::Malformed)
}

fn write(path: &Path, bytes: &[u8; SECRET_BYTES]) -> io::Result<()> {
    for outcome in sweep_stale_temporaries(path, None) {
        match outcome {
            TemporarySweep::Removed { .. } => (),
            TemporarySweep::Failed { source, .. } => return Err(source),
        }
    }
    replace_private_file(path, PrivateFileContents::Bytes(bytes)).map_err(|error| error.source)
}

pub(crate) fn absorb(digest: &mut impl Update, part: &[u8]) {
    digest.update(&(part.len() as u64).to_be_bytes());
    digest.update(part);
}

fn finish(prefix: &str, mac: Hmac<Sha256>) -> String {
    let tail: String = mac.finalize().into_bytes()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{prefix}-{tail}")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use dekopon_core::private_file::temporary_path;

    #[test]
    fn a_cache_key_replacement_preserves_the_old_secret_until_publication_and_retries() {
        let directory = crate::tests::temporary();
        let path = directory.path().join(SECRET_FILE);
        write(&path, &[1; SECRET_BYTES]).unwrap();
        let temporary = temporary_path(&path);
        fs::write(&temporary, b"partial secret").unwrap();
        fs::File::open(&temporary).unwrap().sync_all().unwrap();
        assert_eq!(fs::read(&path).unwrap(), [1; SECRET_BYTES]);

        write(&path, &[2; SECRET_BYTES]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), [2; SECRET_BYTES]);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert!(!temporary.exists());
    }
}
