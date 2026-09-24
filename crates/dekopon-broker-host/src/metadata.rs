use std::{fmt::Write as _, path::PathBuf};

use sha2::{Digest as _, Sha256};

use crate::ProviderManifest;

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedProviderMetadata {
    pub source: PathBuf,
    pub artifact_bytes: u64,
    pub artifact_sha256: String,
    pub manifest: ProviderManifest,
}

#[derive(Eq, PartialEq)]
pub(crate) struct ArtifactIdentity {
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

pub(crate) fn identify_bytes(bytes: &[u8]) -> ArtifactIdentity {
    ArtifactIdentity {
        bytes: bytes.len() as u64,
        sha256: hex_digest(&Sha256::digest(bytes)),
    }
}

pub(crate) fn hex_digest(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}
