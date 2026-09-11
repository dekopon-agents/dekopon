//! Domain-separated SHA-256 derivations for every opaque name and commitment the host writes.
//!
//! Nothing here is secret and nothing here authenticates. Isolation is the broker granting only the
//! caller's own scope and the host binding each handle to that scope's directory; a name anyone
//! who can already list the storage root could recompute changes neither.

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

/// SHA-256 over the domain and every field, each length-prefixed.
///
/// The prefixes make the input injective: no two domains, and no two ways of splitting the same
/// bytes into fields, hash the same message.
pub(crate) fn digest(domain: &str, fields: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in [b"dekopon-storage-domain-v1".as_slice(), domain.as_bytes()]
        .iter()
        .chain(fields)
    {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    #[cfg(test)]
    note_hashed(fields.iter().fold(0_u64, |sum, field| {
        sum.saturating_add(8 + field.len() as u64)
    }));
    hasher.finalize().into()
}

/// Sixty-four lowercase hexadecimal digits: every physical name in the tree has this shape.
pub(crate) fn token(domain: &str, fields: &[&[u8]]) -> String {
    hex(&digest(domain, fields))
}

pub(crate) fn commitment(domain: &str, fields: &[&[u8]]) -> String {
    format!("sha256:{}", token(domain, fields))
}

#[cfg(test)]
thread_local! {
    /// Field bytes hashed on this thread.
    ///
    /// Test-only instrumentation. Hashing cost is a behavior this crate has to hold to—reserving a
    /// positional write must not depend on file size—so it is measured rather than assumed.
    static HASHED_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_hashed(bytes: u64) {
    HASHED_BYTES.with(|cell| cell.set(cell.get().saturating_add(bytes)));
}

/// Field bytes hashed on this thread so far.
#[cfg(test)]
pub(crate) fn hashed_bytes() -> u64 {
    HASHED_BYTES.with(std::cell::Cell::get)
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
        DOMAIN_AUDIT_SCOPE, DOMAIN_AUTHORITY, DOMAIN_CONTENT, DOMAIN_DECISION_EVIDENCE,
        DOMAIN_GENERATION, DOMAIN_LOGICAL_PATH, DOMAIN_NAMESPACE_PATH, DOMAIN_OPERATION_EVIDENCE,
        DOMAIN_OUTPUT_EVIDENCE, DOMAIN_RECORD_ID, token,
    };

    #[test]
    fn domains_never_reuse_one_token() {
        let fields = [b"same".as_slice()];
        let tokens = [
            DOMAIN_NAMESPACE_PATH,
            DOMAIN_LOGICAL_PATH,
            DOMAIN_AUTHORITY,
            DOMAIN_GENERATION,
            DOMAIN_AUDIT_SCOPE,
            DOMAIN_RECORD_ID,
            DOMAIN_CONTENT,
            DOMAIN_DECISION_EVIDENCE,
            DOMAIN_OUTPUT_EVIDENCE,
            DOMAIN_OPERATION_EVIDENCE,
        ]
        .map(|domain| token(domain, &fields));
        assert_eq!(tokens.iter().collect::<BTreeSet<_>>().len(), tokens.len());
    }

    /// The length prefixes are what keep two field lists that concatenate to the same bytes apart.
    #[test]
    fn moving_a_field_boundary_changes_the_token() {
        assert_ne!(
            token(DOMAIN_NAMESPACE_PATH, &[b"ab", b"c"]),
            token(DOMAIN_NAMESPACE_PATH, &[b"a", b"bc"])
        );
    }
}
