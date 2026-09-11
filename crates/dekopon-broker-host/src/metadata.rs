use std::{fmt::Write as _, path::PathBuf};

use sha2::{Digest as _, Sha256};

use crate::ProviderManifest;

/// Metadata retained for one component that was actually compiled into the broker registry.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedProviderMetadata {
    /// Local source file compiled by Wasmtime.
    pub source: PathBuf,
    /// Length of the buffer that was compiled.
    pub artifact_bytes: u64,
    /// Lowercase SHA-256 of the exact bytes that were compiled.
    pub artifact_sha256: String,
    /// Validated manifest returned by the component.
    pub manifest: ProviderManifest,
}

#[derive(Eq, PartialEq)]
pub(crate) struct ArtifactIdentity {
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

/// Identifies the exact buffer a caller is about to hand to Wasmtime.
///
/// Taking bytes rather than a path is the point: a digest computed from a second read cannot prove
/// it describes what Cranelift compiled, and the recorded `artifact_sha256` is published metadata.
pub(crate) fn identify_bytes(bytes: &[u8]) -> ArtifactIdentity {
    let digest = Sha256::digest(bytes);
    let mut sha256 = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut sha256, "{byte:02x}").expect("writing to a String cannot fail");
    }
    ArtifactIdentity {
        bytes: bytes.len() as u64,
        sha256,
    }
}
