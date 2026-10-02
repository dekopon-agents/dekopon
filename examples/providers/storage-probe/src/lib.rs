use dekopon_provider_sdk::clap::Parser;
use dekopon_provider_sdk::provider::{
    Capability, Code, DurableFiles, Failure, Proposal, Provider, Stdout, Storage, Usage,
    durable_files::{Durability, OpenOptions, StorageError},
};
use dekopon_provider_sdk::{EffectKind, RiskLevel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;

struct StorageProbe;
struct Run;

#[derive(Parser)]
#[command(name = "storageprobe")]
struct Args {}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Input {
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<Mode>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum Mode {
    Success,
    ReadOnlyDenial,
    WrongInterfaceDenial,
    QuotaDenial,
    BudgetDenial,
    DropAfterDenial,
}

#[derive(Debug, Eq, PartialEq)]
enum ProbeError {
    StorageError,
    ShortRead,
    SparseWrite,
    Stat,
    Identity,
    RecreatedStat,
    IdentityReused,
    DeleteOnClose,
    Entropy,
    UnexpectedPresentFile,
    UnexpectedResult,
}
impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("storage probe failed")
    }
}
impl Failure for ProbeError {
    fn code(&self) -> Code {
        Code::new(match self {
            Self::StorageError => "storage-error",
            Self::ShortRead => "short-read",
            Self::SparseWrite => "sparse-write",
            Self::Stat => "stat",
            Self::Identity => "identity",
            Self::RecreatedStat => "recreated-stat",
            Self::IdentityReused => "identity-reused",
            Self::DeleteOnClose => "delete-on-close",
            Self::Entropy => "entropy",
            Self::UnexpectedPresentFile => "unexpected-present-file",
            Self::UnexpectedResult => "unexpected-result",
        })
    }
}
fn map(_: StorageError) -> ProbeError {
    ProbeError::StorageError
}

impl Provider for StorageProbe {
    const ID: &'static str = "storage-probe";
    const COMMAND_WORDS: &'static [&'static str] = &["storageprobe"];
    const DESCRIPTION: &'static str = "Exercises every durable-files contract family";
    type Args = Args;
    type Capabilities = (Run,);
    fn propose(_: Args, _: bool) -> Result<Proposal<Self>, Usage> {
        Ok(Proposal::to::<Run>(Input { mode: None }))
    }
}
impl Capability for Run {
    type Provider = StorageProbe;
    const NAME: &'static str = "run";
    const DESCRIPTION: &'static str = "Runs the durable-file conformance sequence";
    const EFFECT: EffectKind = EffectKind::LocalWrite;
    const RISK: RiskLevel = RiskLevel::Medium;
    type Input = Input;
    type Needs = Storage<DurableFiles>;
    type Error = ProbeError;
    fn run(
        input: Input,
        storage: Storage<DurableFiles>,
        out: &mut Stdout,
    ) -> Result<(), ProbeError> {
        let value = match input.mode.unwrap_or(Mode::Success) {
            Mode::Success => run(&storage),
            Mode::ReadOnlyDenial => catch_read_only_denial(&storage),
            Mode::WrongInterfaceDenial => catch_wrong_interface_denial(&storage),
            Mode::QuotaDenial => catch_quota_denial(&storage),
            Mode::BudgetDenial => catch_budget_denial(&storage),
            Mode::DropAfterDenial => drop_after_denial(&storage),
        }?;
        writeln!(out, "{value}").map_err(|_| ProbeError::StorageError)
    }
}

fn run(storage: &Storage<DurableFiles>) -> Result<Value, ProbeError> {
    exercise_open_flags(storage)?;
    expect(
        storage.open("probe.db", OpenOptions::new()),
        StorageError::InvalidArgument,
    )?;
    expect(
        storage.open("probe.db", OpenOptions::new().read(true).create(true)),
        StorageError::InvalidArgument,
    )?;
    expect(
        storage.open(
            "probe.db",
            OpenOptions::new().write(true).create(true).create_new(true),
        ),
        StorageError::InvalidArgument,
    )?;
    let first = storage
        .open(
            "probe.db",
            OpenOptions::new().read(true).write(true).create_new(true),
        )
        .map_err(map)?;
    first.write_at(0, b"abc").map_err(map)?;
    let short = first.read_at(0, 16).map_err(map)?;
    if short != b"abc" {
        return Err(ProbeError::ShortRead);
    }
    first.write_at(8, b"z").map_err(map)?;
    if first.size().map_err(map)? != 9 {
        return Err(ProbeError::SparseWrite);
    }
    first.truncate(16).map_err(map)?;
    for mode in [
        Durability::Data,
        Durability::DataAndMetadata,
        Durability::Full,
    ] {
        first.sync(mode).map_err(map)?;
    }
    expect(
        storage.rename_atomic("probe.db", "renamed.db", false, Durability::Full),
        StorageError::Busy,
    )?;
    expect(
        storage.remove("probe.db", Durability::Full),
        StorageError::Busy,
    )?;
    drop(first);
    storage
        .rename_atomic("probe.db", "renamed.db", false, Durability::Full)
        .map_err(map)?;
    let identity = storage
        .stat("renamed.db")
        .map_err(map)?
        .ok_or(ProbeError::Stat)?
        .identity;
    if identity == 0 {
        return Err(ProbeError::Identity);
    }
    let recreated = storage
        .open(
            "probe.db",
            OpenOptions::new().read(true).write(true).create_new(true),
        )
        .map_err(map)?;
    drop(recreated);
    let recreated_identity = storage
        .stat("probe.db")
        .map_err(map)?
        .ok_or(ProbeError::RecreatedStat)?
        .identity;
    if recreated_identity == identity {
        return Err(ProbeError::IdentityReused);
    }
    storage.remove("probe.db", Durability::Full).map_err(map)?;
    let deleting = storage
        .open(
            "delete.tmp",
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .delete_on_close(true),
        )
        .map_err(map)?;
    drop(deleting);
    if storage.stat("delete.tmp").map_err(map)?.is_some() {
        return Err(ProbeError::DeleteOnClose);
    }
    let entropy = storage.random_bytes(32).map_err(map)?;
    if entropy.len() != 32 {
        return Err(ProbeError::Entropy);
    }
    let _monotonic = storage.monotonic_time_ns().map_err(map)?;
    let _wall = storage.wall_time_ms().map_err(map)?;
    Ok(
        json!({"shortReadBytes":short.len(),"identityNonzero":identity != 0,"entropyBytes":entropy.len(),"clocksCalled":true}),
    )
}
fn catch_read_only_denial(storage: &Storage<DurableFiles>) -> Result<Value, ProbeError> {
    expect(
        storage.open("denied.db", OpenOptions::new().write(true).create_new(true)),
        StorageError::PermissionDenied,
    )?;
    Ok(json!({"caught":"read-only"}))
}
fn catch_wrong_interface_denial(storage: &Storage<DurableFiles>) -> Result<Value, ProbeError> {
    expect(
        storage.stat("wrong-interface.db"),
        StorageError::PermissionDenied,
    )?;
    Ok(json!({"caught":"wrong-interface"}))
}
fn catch_quota_denial(storage: &Storage<DurableFiles>) -> Result<Value, ProbeError> {
    expect(storage.random_bytes(257), StorageError::QuotaExceeded)?;
    Ok(json!({"caught":"quota"}))
}
fn catch_budget_denial(storage: &Storage<DurableFiles>) -> Result<Value, ProbeError> {
    if storage.stat("missing.db").map_err(map)?.is_some() {
        return Err(ProbeError::UnexpectedPresentFile);
    }
    expect(storage.stat("missing.db"), StorageError::QuotaExceeded)?;
    Ok(json!({"caught":"budget"}))
}
fn drop_after_denial(storage: &Storage<DurableFiles>) -> Result<Value, ProbeError> {
    let deleting = storage
        .open(
            "drop-denied.tmp",
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .delete_on_close(true),
        )
        .map_err(map)?;
    deleting.write_at(0, b"provisional").map_err(map)?;
    expect(storage.random_bytes(257), StorageError::QuotaExceeded)?;
    drop(deleting);
    Ok(json!({"caught":"drop-after-denial"}))
}
fn exercise_open_flags(storage: &Storage<DurableFiles>) -> Result<(), ProbeError> {
    for mask in 0_u8..32 {
        let read = mask & 1 != 0;
        let write = mask & 2 != 0;
        let create = mask & 4 != 0;
        let create_new = mask & 8 != 0;
        let delete_on_close = mask & 16 != 0;
        let options = OpenOptions::new()
            .read(read)
            .write(write)
            .create(create)
            .create_new(create_new)
            .delete_on_close(delete_on_close);
        let name = format!("flags-{mask}.db");
        let invalid = (!read && !write)
            || (create && create_new)
            || ((create || create_new || delete_on_close) && !write);
        let result = storage.open(&name, options);
        if invalid {
            expect(result, StorageError::InvalidArgument)?;
        } else if create || create_new {
            let file = result.map_err(map)?;
            drop(file);
            if !delete_on_close {
                storage.remove(&name, Durability::Full).map_err(map)?;
            }
        } else {
            expect(result, StorageError::NotFound)?;
        }
    }
    Ok(())
}
fn expect<T>(result: Result<T, StorageError>, expected: StorageError) -> Result<(), ProbeError> {
    match result {
        Err(actual) if actual == expected => Ok(()),
        _ => Err(ProbeError::UnexpectedResult),
    }
}

dekopon_provider_sdk::export!(StorageProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use dekopon_provider_sdk::CommandRunOutcome;
    use dekopon_provider_sdk::provider;
    #[test]
    fn storage_failures_keep_distinct_typed_codes() {
        assert_eq!(ProbeError::ShortRead.code(), Code::new("short-read"));
        assert_eq!(
            ProbeError::UnexpectedResult.code(),
            Code::new("unexpected-result")
        );
        assert!(matches!(
            map(StorageError::PermissionDenied),
            ProbeError::StorageError
        ));
    }

    #[test]
    fn typed_dispatch_and_closed_modes() {
        let manifest = provider::manifest::<StorageProbe>().unwrap();
        assert_eq!(manifest.capabilities[0].id.as_str(), "storage-probe.run");
        assert_eq!(manifest.command_words, ["storageprobe"]);
        assert!(
            matches!(provider::command::<StorageProbe>(&[], false), CommandRunOutcome::Proposed { input, .. } if input == json!({}))
        );
        assert_eq!(
            manifest.capabilities[0].input_schema["additionalProperties"],
            false
        );
    }
}
