//! Strict storage ceilings and relationship validation.

use serde::{Deserialize, Serialize};
use thiserror::Error;

const TIB: u64 = 1024 * 1024 * 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// Broker-owned process and invocation storage ceilings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StorageLimits {
    pub max_root_bytes: u64,
    pub max_namespaces: u64,
    pub max_namespace_bytes: u64,
    pub max_files_per_namespace: u64,
    pub max_file_bytes: u64,
    pub max_open_handles: u64,
    pub max_handles_per_invocation: u64,
    pub max_host_calls_per_invocation: u64,
    pub max_read_bytes_per_call: u64,
    pub max_read_bytes_per_invocation: u64,
    pub max_write_bytes_per_call: u64,
    pub max_write_bytes_per_invocation: u64,
    pub max_entropy_bytes_per_call: u64,
    pub max_entropy_bytes_per_invocation: u64,
    pub lock_timeout_ms: u64,
    pub finalization_budget_ms: u64,
    /// Compatibility spelling: bounds concurrently active invocation handles.
    pub max_pending_transactions: u64,
    pub startup_max_entries: u64,
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self {
            max_root_bytes: 2 * GIB,
            max_namespaces: 4_096,
            max_namespace_bytes: 64 * MIB,
            max_files_per_namespace: 64,
            max_file_bytes: 16 * MIB,
            max_open_handles: 256,
            max_handles_per_invocation: 32,
            max_host_calls_per_invocation: 4_096,
            max_read_bytes_per_call: 256 * 1024,
            max_read_bytes_per_invocation: 16 * MIB,
            // JSONL replacement is one host call. The default memory compaction target is
            // 8 MiB, so the host-call ceiling must admit that complete replacement rather than
            // letting a valid store become permanently unable to compact.
            max_write_bytes_per_call: 16 * MIB,
            max_write_bytes_per_invocation: 16 * MIB,
            max_entropy_bytes_per_call: 256,
            max_entropy_bytes_per_invocation: 4_096,
            lock_timeout_ms: 5_000,
            finalization_budget_ms: 5_000,
            max_pending_transactions: 64,
            startup_max_entries: 100_000,
        }
    }
}

impl StorageLimits {
    /// Validates compile-time ceilings, checked relationships, and restart admissibility.
    pub fn validate(&self) -> Result<(), StorageConfigError> {
        let positive = [
            ("maxRootBytes", self.max_root_bytes),
            ("maxNamespaces", self.max_namespaces),
            ("maxNamespaceBytes", self.max_namespace_bytes),
            ("maxFilesPerNamespace", self.max_files_per_namespace),
            ("maxFileBytes", self.max_file_bytes),
            ("maxOpenHandles", self.max_open_handles),
            ("maxHandlesPerInvocation", self.max_handles_per_invocation),
            (
                "maxHostCallsPerInvocation",
                self.max_host_calls_per_invocation,
            ),
            ("maxReadBytesPerCall", self.max_read_bytes_per_call),
            (
                "maxReadBytesPerInvocation",
                self.max_read_bytes_per_invocation,
            ),
            ("maxWriteBytesPerCall", self.max_write_bytes_per_call),
            (
                "maxWriteBytesPerInvocation",
                self.max_write_bytes_per_invocation,
            ),
            ("maxEntropyBytesPerCall", self.max_entropy_bytes_per_call),
            (
                "maxEntropyBytesPerInvocation",
                self.max_entropy_bytes_per_invocation,
            ),
            ("lockTimeoutMs", self.lock_timeout_ms),
            ("finalizationBudgetMs", self.finalization_budget_ms),
            ("maxPendingTransactions", self.max_pending_transactions),
            ("startupMaxEntries", self.startup_max_entries),
        ];
        if let Some((field, _)) = positive.into_iter().find(|(_, value)| *value == 0) {
            return Err(StorageConfigError::Zero { field });
        }

        let ceilings = [
            ("maxRootBytes", self.max_root_bytes, TIB),
            ("maxNamespaces", self.max_namespaces, 65_536),
            ("maxNamespaceBytes", self.max_namespace_bytes, GIB),
            ("maxFilesPerNamespace", self.max_files_per_namespace, 1_024),
            ("maxFileBytes", self.max_file_bytes, 256 * MIB),
            ("maxOpenHandles", self.max_open_handles, 4_096),
            (
                "maxHandlesPerInvocation",
                self.max_handles_per_invocation,
                256,
            ),
            (
                "maxHostCallsPerInvocation",
                self.max_host_calls_per_invocation,
                1_000_000,
            ),
            (
                "maxReadBytesPerCall",
                self.max_read_bytes_per_call,
                16 * MIB,
            ),
            (
                "maxReadBytesPerInvocation",
                self.max_read_bytes_per_invocation,
                GIB,
            ),
            (
                "maxWriteBytesPerCall",
                self.max_write_bytes_per_call,
                16 * MIB,
            ),
            (
                "maxWriteBytesPerInvocation",
                self.max_write_bytes_per_invocation,
                GIB,
            ),
            (
                "maxEntropyBytesPerCall",
                self.max_entropy_bytes_per_call,
                4 * 1024,
            ),
            (
                "maxEntropyBytesPerInvocation",
                self.max_entropy_bytes_per_invocation,
                64 * 1024,
            ),
            ("lockTimeoutMs", self.lock_timeout_ms, 60_000),
            ("finalizationBudgetMs", self.finalization_budget_ms, 60_000),
            (
                "maxPendingTransactions",
                self.max_pending_transactions,
                1_024,
            ),
            ("startupMaxEntries", self.startup_max_entries, 1_000_000),
        ];
        if let Some((field, value, maximum)) = ceilings
            .into_iter()
            .find(|(_, value, maximum)| value > maximum)
        {
            return Err(StorageConfigError::AboveCeiling {
                field,
                value,
                maximum,
            });
        }

        let relationships = [
            (
                self.max_read_bytes_per_call <= self.max_read_bytes_per_invocation,
                "read call <= invocation",
            ),
            (
                self.max_write_bytes_per_call <= self.max_write_bytes_per_invocation,
                "write call <= invocation",
            ),
            (
                self.max_entropy_bytes_per_call <= self.max_entropy_bytes_per_invocation,
                "entropy call <= invocation",
            ),
            (
                self.max_file_bytes <= self.max_namespace_bytes,
                "file <= namespace",
            ),
            (
                self.max_namespace_bytes <= self.max_root_bytes,
                "namespace <= root",
            ),
            (
                self.max_handles_per_invocation <= self.max_open_handles,
                "invocation handles <= process handles",
            ),
        ];
        if let Some((_, relationship)) = relationships.into_iter().find(|(valid, _)| !valid) {
            return Err(StorageConfigError::Relationship { relationship });
        }

        // `startupMaxEntries` is also enforced as the live root-wide entry cap. It may therefore
        // be lower than the product of independent namespace/file ceilings without admitting a
        // store that cannot restart. One namespace must still be representable in full: the three
        // root entries, then the base, its lease, one generation and that generation's data
        // directory, before any of the namespace's own files.
        let one_namespace = self
            .max_files_per_namespace
            .checked_add(7)
            .ok_or(StorageConfigError::Arithmetic)?;
        if one_namespace > self.startup_max_entries {
            return Err(StorageConfigError::RestartCapacity {
                required: one_namespace,
                configured: self.startup_max_entries,
            });
        }
        Ok(())
    }
}

/// Invalid storage configuration.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StorageConfigError {
    #[error("storage field {field} must be greater than zero")]
    Zero { field: &'static str },
    #[error("storage field {field} value {value} exceeds compile-time ceiling {maximum}")]
    AboveCeiling {
        field: &'static str,
        value: u64,
        maximum: u64,
    },
    #[error("invalid storage relationship: {relationship}")]
    Relationship { relationship: &'static str },
    #[error("storage limit arithmetic overflowed")]
    Arithmetic,
    #[error("startupMaxEntries {configured} cannot scan runtime-admissible {required} entries")]
    RestartCapacity { required: u64, configured: u64 },
}

#[cfg(test)]
mod tests {
    use super::{StorageConfigError, StorageLimits};

    #[test]
    fn retired_gc_recovery_and_quarantine_keys_are_unknown() {
        let defaults = StorageLimits::default();
        let valid = serde_json::to_value(&defaults).expect("serialize defaults");
        assert_eq!(
            serde_json::from_value::<StorageLimits>(valid.clone()).expect("valid defaults"),
            defaults
        );
        for key in [
            "startupMaxTransactions",
            "retiredGenerationGraceMs",
            "retiredGenerationTtlMs",
            "inactiveNamespaceTtlMs",
            "gcIntervalMs",
            "gcMaxNamespacesPerPass",
            "gcMaxBytesPerPass",
            "maxQuarantinedNamespaces",
        ] {
            let mut value = valid.clone();
            value
                .as_object_mut()
                .expect("limits object")
                .insert(key.to_owned(), 1.into());
            let error =
                serde_json::from_value::<StorageLimits>(value).expect_err("retired key refused");
            assert!(
                error
                    .to_string()
                    .contains(&format!("unknown field `{key}`")),
                "{error}"
            );
        }
    }

    #[test]
    fn defaults_are_composed() {
        StorageLimits::default()
            .validate()
            .expect("defaults validate");
    }

    #[test]
    fn restart_scanner_must_cover_runtime_state() {
        let mut limits = StorageLimits::default();
        limits.startup_max_entries = limits.max_files_per_namespace;
        assert!(matches!(
            limits.validate(),
            Err(StorageConfigError::RestartCapacity { .. })
        ));
    }
}
