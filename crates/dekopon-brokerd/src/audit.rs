//! Offline verification of a durable audit chain.
//!
//! The broker verifies its own log at startup and refuses to listen behind a broken chain, which
//! left `dekopon_broker::verify_audit_chain` with no operator path at all: the only way to run it
//! was to start the daemon and read a refusal. Answering "is the retained log still intact" for a
//! log the broker is not currently serving needs a check that binds no socket and takes no lock.
//!
//! Reading is bounded. The per-record ceiling is the hard frame bound, which is the largest
//! `auditMaxLineBytes` any configuration may set, so no log the broker would accept is refused
//! here. Unlike the broker's startup scan this one retains the records, because
//! `verify_audit_chain` verifies a slice — which is what the record cap bounds.

use std::{
    fs::File,
    io::{self, BufRead as _, BufReader, Read as _},
    path::{Path, PathBuf},
};

use dekopon_broker::{
    AuditIntegrityError, AuditRecord, DEFAULT_MAX_AUDIT_RECORDS, verify_audit_chain,
};
use dekopon_broker_protocol::HARD_MAX_FRAME_BYTES;
use serde::Serialize;
use thiserror::Error;

/// What one offline verification found.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditVerification {
    /// The verified log.
    pub path: PathBuf,
    /// Records the chain contains.
    pub records: usize,
    /// Hash of the last record, absent for an empty log.
    pub head: Option<String>,
}

/// Why an audit log could not be verified.
///
/// The distinction an operator acts on is *the file could not be read* versus *the chain
/// disagrees with itself*. The first is a path, permission, or truncation problem; the second is
/// the durable record contradicting the authority it exists to prove.
#[derive(Debug, Error)]
pub enum AuditVerificationError {
    /// The log could not be opened or read.
    #[error("could not read audit log {}", path.display())]
    Io {
        /// The path that was read.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// One record exceeded the largest line any broker configuration may accept.
    #[error("audit record {line} exceeds the {maximum}-byte record ceiling")]
    LineTooLong {
        /// One-based record number.
        line: usize,
        /// The ceiling that was exceeded.
        maximum: usize,
    },
    /// The final record carries no newline, which is what a partial append looks like.
    #[error("audit record {line} is unterminated")]
    UnterminatedRecord {
        /// One-based record number.
        line: usize,
    },
    /// The log holds more records than this check will hold in memory at once.
    #[error("audit log holds more than {maximum} records")]
    TooManyRecords {
        /// The record cap that was exceeded.
        maximum: usize,
    },
    /// One line is not a valid audit record.
    #[error("audit record {line} is not a valid record")]
    InvalidRecord {
        /// One-based record number.
        line: usize,
        /// The underlying decode failure.
        #[source]
        source: serde_json::Error,
    },
    /// Sequence numbers, previous-hash links, or record hashes disagree.
    #[error("the audit chain is broken")]
    Chain(#[source] AuditIntegrityError),
}

/// Verifies every retained record in a durable JSONL audit log.
///
/// Opens the file read-only and takes no lock, so it answers for a log a running broker holds open
/// as well as for a retained copy.
pub fn verify_audit_file(
    path: impl AsRef<Path>,
) -> Result<AuditVerification, AuditVerificationError> {
    let path = path.as_ref().to_path_buf();
    let file = File::open(&path).map_err(|source| AuditVerificationError::Io {
        path: path.clone(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    let mut records: Vec<AuditRecord> = Vec::new();

    while let Some(line) = read_bounded_line(&mut reader, &path, records.len() + 1)? {
        if records.len() >= DEFAULT_MAX_AUDIT_RECORDS {
            return Err(AuditVerificationError::TooManyRecords {
                maximum: DEFAULT_MAX_AUDIT_RECORDS,
            });
        }
        let record = serde_json::from_slice::<AuditRecord>(&line).map_err(|source| {
            AuditVerificationError::InvalidRecord {
                line: records.len() + 1,
                source,
            }
        })?;
        records.push(record);
    }

    verify_audit_chain(&records).map_err(AuditVerificationError::Chain)?;
    Ok(AuditVerification {
        path,
        records: records.len(),
        head: records.last().map(|record| record.record_hash.clone()),
    })
}

/// Reads one newline-terminated record, refusing an oversized one rather than buffering it.
fn read_bounded_line(
    reader: &mut BufReader<File>,
    path: &Path,
    line_number: usize,
) -> Result<Option<Vec<u8>>, AuditVerificationError> {
    let mut line = Vec::new();
    // One byte past the ceiling separates "exactly at the limit" from "over it" while still never
    // holding more than one oversized record's worth of bytes.
    let limit = u64::try_from(HARD_MAX_FRAME_BYTES)
        .expect("the frame ceiling fits in u64")
        .saturating_add(1);
    let read = (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut line)
        .map_err(|source| AuditVerificationError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        if line.len() > HARD_MAX_FRAME_BYTES {
            return Err(AuditVerificationError::LineTooLong {
                line: line_number,
                maximum: HARD_MAX_FRAME_BYTES,
            });
        }
        return Err(AuditVerificationError::UnterminatedRecord { line: line_number });
    }
    line.pop();
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _};

    use dekopon_broker::{AuditEvent, AuditLog as _, FileAuditLog};
    use dekopon_core::{Actor, AgentId, CapabilityId, InvocationId, PrincipalId, TraceId};

    use super::{AuditVerificationError, verify_audit_file};

    fn decision(invocation: &str) -> AuditEvent {
        AuditEvent::Decision {
            invocation: invocation
                .parse::<InvocationId>()
                .expect("valid invocation fixture"),
            trace: "trace-audit-verify"
                .parse::<TraceId>()
                .expect("valid trace fixture"),
            principal: Some(
                "caller"
                    .parse::<PrincipalId>()
                    .expect("valid principal fixture"),
            ),
            actor: Some(Actor::Agent {
                agent: "audit-verify-test"
                    .parse::<AgentId>()
                    .expect("valid agent fixture"),
            }),
            via: None,
            attested_subject: None,
            capability: "echo.echo"
                .parse::<CapabilityId>()
                .expect("valid capability fixture"),
            secret: None,
            secret_sink: None,
            provider: None,
            authorized_by: Some(
                "broker"
                    .parse::<PrincipalId>()
                    .expect("valid principal fixture"),
            ),
            decision_id: format!("decision-{invocation}"),
            policy_revision: Some("policy-audit-verify".to_owned()),
            policy_ids: Vec::new(),
            policy_digest: None,
            allowed: false,
            reason: Some("policy-denied".to_owned()),
            decision_digest: format!("sha256:{}", "a".repeat(64)),
            storage_scope_commitment: None,
            storage: None,
        }
    }

    /// Writes a real two-record chain and returns its directory and expected head.
    async fn chain() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let directory = tempfile::tempdir().expect("create audit fixture");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("secure audit directory");
        let path = directory.path().join("audit.jsonl");
        let audit = FileAuditLog::open(&path, 8, 16 * 1024)
            .await
            .expect("open audit");
        audit
            .append(decision("invoke-audit-one"))
            .await
            .expect("append first record");
        audit
            .append(decision("invoke-audit-two"))
            .await
            .expect("append second record");
        let (count, head) = audit.checkpoint().await;
        assert_eq!(count, 2);
        drop(audit);
        (directory, path, head.expect("a nonempty chain has a head"))
    }

    #[tokio::test]
    async fn reports_the_record_count_and_head_of_an_intact_chain() {
        let (_directory, path, head) = chain().await;

        let verification = verify_audit_file(&path).expect("an intact chain verifies");

        assert_eq!(verification.path, path);
        assert_eq!(verification.records, 2);
        assert_eq!(verification.head, Some(head));
    }

    /// The whole point of the command: a record edited in place still parses, and only the hash
    /// chain says it was edited.
    #[tokio::test]
    async fn a_tampered_record_is_reported_as_a_broken_chain() {
        let (_directory, path, _head) = chain().await;
        let original = fs::read_to_string(&path).expect("read chain");
        fs::write(
            &path,
            original.replace("\"allowed\":false", "\"allowed\":true"),
        )
        .expect("rewrite tampered chain");

        let error = verify_audit_file(&path).expect_err("a tampered record must not verify");

        assert!(
            matches!(error, AuditVerificationError::Chain(_)),
            "{error:?}"
        );
    }

    /// A partial append is what an interrupted write leaves behind, and it is not a broken chain.
    #[tokio::test]
    async fn an_unterminated_final_record_is_named_as_such() {
        let (_directory, path, _head) = chain().await;
        let mut content = fs::read_to_string(&path).expect("read chain");
        content.push_str("{\"sequence\":3");
        fs::write(&path, content).expect("write partial append");

        let error = verify_audit_file(&path).expect_err("a partial append must not verify");

        assert!(
            matches!(
                error,
                AuditVerificationError::UnterminatedRecord { line: 3 }
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_log_verifies_with_no_head() {
        let directory = tempfile::tempdir().expect("create audit fixture");
        let path = directory.path().join("audit.jsonl");
        fs::write(&path, "").expect("write empty log");

        let verification = verify_audit_file(&path).expect("an empty chain verifies");

        assert_eq!(verification.records, 0);
        assert_eq!(verification.head, None);
    }

    #[tokio::test]
    async fn a_missing_log_names_the_path_it_could_not_read() {
        let directory = tempfile::tempdir().expect("create audit fixture");
        let path = directory.path().join("absent.jsonl");

        let error = verify_audit_file(&path).expect_err("a missing log cannot verify");

        assert!(
            matches!(&error, AuditVerificationError::Io { path: reported, .. } if *reported == path),
            "{error:?}"
        );
    }
}
