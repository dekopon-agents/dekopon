//! Namespace-key loading and domain-separated commitments.

use std::{
    fmt,
    fs::File,
    io::Read as _,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::Path,
};

use rustix::fs::{Mode, OFlags};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::StorageHostError;

pub(crate) const DOMAIN_NAMESPACE_PATH: &str = "namespace-path-v1";
pub(crate) const DOMAIN_LOGICAL_PATH: &str = "logical-name-path-v1";
pub(crate) const DOMAIN_AUTHORITY: &str = "authority-commitment-v1";
pub(crate) const DOMAIN_GENERATION: &str = "generation-token-v1";
pub(crate) const DOMAIN_AUDIT_SCOPE: &str = "audit-scope-commitment-v1";
pub(crate) const DOMAIN_RECORD_ID: &str = "record-id-v1";
pub(crate) const DOMAIN_CONTENT: &str = "content-dedup-commitment-v1";
pub(crate) const DOMAIN_DECISION_EVIDENCE: &str = "storage-decision-evidence-v1";
pub(crate) const DOMAIN_OUTPUT_EVIDENCE: &str = "storage-output-evidence-v1";
pub(crate) const DOMAIN_OPERATION_EVIDENCE: &str = "storage-operation-evidence-v1";
pub(crate) const DOMAIN_LIFECYCLE: &str = "storage-lifecycle-marker-v1";
pub(crate) const DOMAIN_MANIFEST: &str = "transaction-manifest-v1";
pub(crate) const DOMAIN_TRANSACTION: &str = "transaction-path-v1";

const MAX_KEY_FILE_BYTES: u64 = 4 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct KeyDocument {
    api_version: String,
    key: String,
}

/// Process-local namespace key. Every formatter is fully redacted.
pub(crate) struct StorageKey([u8; 32]);

impl fmt::Debug for StorageKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StorageKey([REDACTED])")
    }
}

impl StorageKey {
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn load(path: &Path) -> Result<Self, StorageHostError> {
        let parent = path
            .parent()
            .ok_or_else(|| StorageHostError::UnsafeKeyFile {
                path: path.to_path_buf(),
            })?;
        let name = path
            .file_name()
            .ok_or_else(|| StorageHostError::UnsafeKeyFile {
                path: path.to_path_buf(),
            })?;
        let parent_fd = rustix::fs::open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| StorageHostError::KeyIo {
            path: parent.to_path_buf(),
            source: std::io::Error::from(source),
        })?;
        let parent_file = File::from(parent_fd);
        let parent_metadata = parent_file
            .metadata()
            .map_err(|source| StorageHostError::KeyIo {
                path: parent.to_path_buf(),
                source,
            })?;
        if !parent_metadata.is_dir()
            || parent_metadata.uid() != rustix::process::geteuid().as_raw()
            || parent_metadata.permissions().mode() & 0o022 != 0
        {
            return Err(StorageHostError::UnsafeKeyFile {
                path: path.to_path_buf(),
            });
        }
        let fd = rustix::fs::openat(
            &parent_file,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| StorageHostError::KeyIo {
            path: path.to_path_buf(),
            source: std::io::Error::from(source),
        })?;
        let file = File::from(fd);
        let metadata = file.metadata().map_err(|source| StorageHostError::KeyIo {
            path: path.to_path_buf(),
            source,
        })?;
        let expected_uid = rustix::process::geteuid().as_raw();
        if !metadata.is_file()
            || metadata.uid() != expected_uid
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o777 != 0o600
            || metadata.len() > MAX_KEY_FILE_BYTES
        {
            return Err(StorageHostError::UnsafeKeyFile {
                path: path.to_path_buf(),
            });
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_KEY_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| StorageHostError::KeyIo {
                path: path.to_path_buf(),
                source,
            })?;
        if bytes.len() as u64 > MAX_KEY_FILE_BYTES {
            return Err(StorageHostError::UnsafeKeyFile {
                path: path.to_path_buf(),
            });
        }
        let document: KeyDocument =
            serde_yaml::from_slice(&bytes).map_err(|_| StorageHostError::InvalidKeyFile)?;
        if document.api_version != "dekopon.dev/storage-key/v1alpha1"
            || document.key.len() != 64
            || !document
                .key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(StorageHostError::InvalidKeyFile);
        }
        let mut key = [0_u8; 32];
        for (index, output) in key.iter_mut().enumerate() {
            *output = u8::from_str_radix(&document.key[index * 2..index * 2 + 2], 16)
                .map_err(|_| StorageHostError::InvalidKeyFile)?;
        }
        Ok(Self(key))
    }

    /// HMAC-SHA-256 with one sub-key per explicit domain and length-prefixed fields.
    pub(crate) fn bytes(&self, domain: &str, fields: &[&[u8]]) -> [u8; 32] {
        let domain_key = hmac_sha256(
            &self.0,
            &encoded_fields(&[b"dekopon-storage-domain-v1".as_slice(), domain.as_bytes()]),
        );
        hmac_sha256(&domain_key, &encoded_fields(fields))
    }

    pub(crate) fn token(&self, domain: &str, fields: &[&[u8]]) -> String {
        hex(&self.bytes(domain, fields))
    }

    pub(crate) fn commitment(&self, domain: &str, fields: &[&[u8]]) -> String {
        format!("hmac-sha256:{}", self.token(domain, fields))
    }
}

pub(crate) fn encoded_fields(fields: &[&[u8]]) -> Vec<u8> {
    let capacity = fields
        .iter()
        .fold(0_usize, |sum, field| sum.saturating_add(8 + field.len()));
    let mut encoded = Vec::with_capacity(capacity);
    for field in fields {
        encoded.extend_from_slice(&(field.len() as u64).to_be_bytes());
        encoded.extend_from_slice(field);
    }
    encoded
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut normalized = [0_u8; BLOCK];
    if key.len() > BLOCK {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK];
    let mut outer_pad = [0x5c_u8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[(byte >> 4) as usize]));
        output.push(char::from(DIGITS[(byte & 0x0f) as usize]));
    }
    output
}

pub(crate) fn random_bytes(length: usize) -> Result<Vec<u8>, StorageHostError> {
    let mut bytes = vec![0_u8; length];
    getrandom::fill(&mut bytes).map_err(|source| StorageHostError::Entropy {
        source: std::io::Error::other(source),
    })?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        DOMAIN_AUDIT_SCOPE, DOMAIN_CONTENT, DOMAIN_DECISION_EVIDENCE, DOMAIN_GENERATION,
        DOMAIN_LIFECYCLE, DOMAIN_LOGICAL_PATH, DOMAIN_MANIFEST, DOMAIN_NAMESPACE_PATH,
        DOMAIN_OPERATION_EVIDENCE, DOMAIN_OUTPUT_EVIDENCE, DOMAIN_RECORD_ID, DOMAIN_TRANSACTION,
        StorageKey,
    };

    #[test]
    fn domains_never_reuse_one_commitment() {
        let key = StorageKey([7; 32]);
        let fields = [b"same".as_slice()];
        let commitments = [
            DOMAIN_NAMESPACE_PATH,
            DOMAIN_LOGICAL_PATH,
            super::DOMAIN_AUTHORITY,
            DOMAIN_GENERATION,
            DOMAIN_AUDIT_SCOPE,
            DOMAIN_RECORD_ID,
            DOMAIN_CONTENT,
            DOMAIN_DECISION_EVIDENCE,
            DOMAIN_OUTPUT_EVIDENCE,
            DOMAIN_OPERATION_EVIDENCE,
            DOMAIN_LIFECYCLE,
            DOMAIN_MANIFEST,
            DOMAIN_TRANSACTION,
        ]
        .map(|domain| key.token(domain, &fields));
        assert_eq!(
            commitments.iter().collect::<BTreeSet<_>>().len(),
            commitments.len()
        );
    }
}
