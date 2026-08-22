use dekopon_provider_sdk::{
    CapabilityId, CommandInvocation, EffectKind, Idempotency, Provider, ProviderApiVersion,
    ProviderCapability, ProviderError, ProviderManifest, RiskLevel,
};
use dekopon_provider_storage::durable_files::{
    self as storage, Durability, LockLevel, OpenOptions, StorageError,
};
use serde_json::{Value, json};

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

struct StorageProbe;

impl Provider for StorageProbe {
    fn manifest() -> ProviderManifest {
        ProviderManifest {
            api_version: ProviderApiVersion::V1Alpha1,
            id: "storage-probe".parse().expect("static provider"),
            description: "Exercises every durable-files contract family".to_owned(),
            command_words: vec!["storageprobe".to_owned()],
            capabilities: vec![ProviderCapability {
                id: "storage-probe.run".parse().expect("static capability"),
                description: "Runs the durable-file conformance sequence".to_owned(),
                effect: EffectKind::LocalWrite,
                risk: RiskLevel::Medium,
                idempotency: Idempotency::Conditional,
                input_schema: json!({
                    "type":"object",
                    "properties": {
                        "mode": {
                            "type":"string",
                            "enum":[
                                "success", "read-only-denial", "wrong-interface-denial",
                                "quota-denial", "budget-denial", "drop-after-denial"
                            ]
                        }
                    },
                    "additionalProperties":false
                }),
            }],
        }
    }

    fn invoke(capability: &CapabilityId, input: Value) -> Result<Value, ProviderError> {
        if capability.as_str() != "storage-probe.run" {
            return Err(failure("invalid-input"));
        }
        let object = input.as_object().ok_or_else(|| failure("invalid-input"))?;
        if object.len() > 1 || object.keys().any(|key| key != "mode") {
            return Err(failure("invalid-input"));
        }
        match object
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("success")
        {
            "success" => run(),
            "read-only-denial" => catch_read_only_denial(),
            "wrong-interface-denial" => catch_wrong_interface_denial(),
            "quota-denial" => catch_quota_denial(),
            "budget-denial" => catch_budget_denial(),
            "drop-after-denial" => drop_after_denial(),
            _ => Err(failure("invalid-input")),
        }
    }

    fn resolve_command(argv: &[String]) -> Result<CommandInvocation, ProviderError> {
        if !argv.is_empty() {
            return Err(failure("invalid-command"));
        }
        Ok(CommandInvocation {
            capability: "storage-probe.run".parse().expect("static capability"),
            input: json!({}),
        })
    }
}

fn run() -> Result<Value, ProviderError> {
    exercise_open_flags()?;
    expect(
        storage::open("probe.db", OpenOptions::new()),
        StorageError::InvalidArgument,
    )?;
    expect(
        storage::open("probe.db", OpenOptions::new().read(true).create(true)),
        StorageError::InvalidArgument,
    )?;
    expect(
        storage::open(
            "probe.db",
            OpenOptions::new().write(true).create(true).create_new(true),
        ),
        StorageError::InvalidArgument,
    )?;

    let first = storage::open(
        "probe.db",
        OpenOptions::new().read(true).write(true).create_new(true),
    )
    .map_err(map)?;
    first.write_at(0, b"abc").map_err(map)?;
    let short = first.read_at(0, 16).map_err(map)?;
    if short != b"abc" {
        return Err(failure("short-read"));
    }
    first.write_at(8, b"z").map_err(map)?;
    if first.size().map_err(map)? != 9 {
        return Err(failure("sparse-write"));
    }
    first.truncate(16).map_err(map)?;
    for mode in [
        Durability::Data,
        Durability::DataAndMetadata,
        Durability::Full,
    ] {
        first.sync(mode).map_err(map)?;
    }

    let second =
        storage::open("probe.db", OpenOptions::new().read(true).write(true)).map_err(map)?;
    let third =
        storage::open("probe.db", OpenOptions::new().read(true).write(true)).map_err(map)?;
    first.lock(LockLevel::Shared).map_err(map)?;
    second.lock(LockLevel::Shared).map_err(map)?;
    first.lock(LockLevel::Reserved).map_err(map)?;
    expect(second.lock(LockLevel::Reserved), StorageError::Busy)?;
    if !second.check_reserved_lock().map_err(map)? {
        return Err(failure("reserved-check"));
    }
    first.lock(LockLevel::Pending).map_err(map)?;
    expect(third.lock(LockLevel::Shared), StorageError::Busy)?;
    expect(first.lock(LockLevel::Exclusive), StorageError::Busy)?;
    second.unlock(LockLevel::None).map_err(map)?;
    first.lock(LockLevel::Exclusive).map_err(map)?;
    first.unlock(LockLevel::None).map_err(map)?;
    drop(third);

    expect(
        storage::rename_atomic("probe.db", "renamed.db", false, Durability::Full),
        StorageError::Busy,
    )?;
    expect(
        storage::remove("probe.db", Durability::Full),
        StorageError::Busy,
    )?;
    drop(second);
    drop(first);
    storage::rename_atomic("probe.db", "renamed.db", false, Durability::Full).map_err(map)?;
    let identity = storage::stat("renamed.db")
        .map_err(map)?
        .ok_or_else(|| failure("stat"))?
        .identity;
    if identity == 0 {
        return Err(failure("identity"));
    }
    let recreated = storage::open(
        "probe.db",
        OpenOptions::new().read(true).write(true).create_new(true),
    )
    .map_err(map)?;
    drop(recreated);
    let recreated_identity = storage::stat("probe.db")
        .map_err(map)?
        .ok_or_else(|| failure("recreated-stat"))?
        .identity;
    if recreated_identity == identity {
        return Err(failure("identity-reused"));
    }
    storage::remove("probe.db", Durability::Full).map_err(map)?;

    let deleting = storage::open(
        "delete.tmp",
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .delete_on_close(true),
    )
    .map_err(map)?;
    drop(deleting);
    if storage::stat("delete.tmp").map_err(map)?.is_some() {
        return Err(failure("delete-on-close"));
    }

    let entropy = storage::random_bytes(32).map_err(map)?;
    if entropy.len() != 32 {
        return Err(failure("entropy"));
    }
    let _monotonic = storage::monotonic_time_ns().map_err(map)?;
    let _wall = storage::wall_time_ms().map_err(map)?;
    Ok(json!({
        "shortReadBytes": short.len(),
        "identityNonzero": identity != 0,
        "entropyBytes": entropy.len(),
        "clocksCalled": true,
    }))
}

fn catch_read_only_denial() -> Result<Value, ProviderError> {
    expect(
        storage::open("denied.db", OpenOptions::new().write(true).create_new(true)),
        StorageError::PermissionDenied,
    )?;
    Ok(json!({"caught": "read-only"}))
}

fn catch_wrong_interface_denial() -> Result<Value, ProviderError> {
    expect(
        storage::stat("wrong-interface.db"),
        StorageError::PermissionDenied,
    )?;
    Ok(json!({"caught": "wrong-interface"}))
}

fn catch_quota_denial() -> Result<Value, ProviderError> {
    // One byte above the storage host's default entropy-per-call ceiling.
    expect(storage::random_bytes(257), StorageError::QuotaExceeded)?;
    Ok(json!({"caught": "quota"}))
}

fn catch_budget_denial() -> Result<Value, ProviderError> {
    // The integration host gives this mode a one-call budget. Not-found remains non-terminal;
    // the second call is caught as quota and must still reject the whole invocation.
    if storage::stat("missing.db").map_err(map)?.is_some() {
        return Err(failure("unexpected-present-file"));
    }
    expect(storage::stat("missing.db"), StorageError::QuotaExceeded)?;
    Ok(json!({"caught": "budget"}))
}

fn drop_after_denial() -> Result<Value, ProviderError> {
    let deleting = storage::open(
        "drop-denied.tmp",
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .delete_on_close(true),
    )
    .map_err(map)?;
    deleting.write_at(0, b"provisional").map_err(map)?;
    expect(storage::random_bytes(257), StorageError::QuotaExceeded)?;
    // Resource drop executes after the guest caught the terminal quota error. It may release native
    // accounting but must never turn delete-on-close into an authorized committed mutation.
    drop(deleting);
    Ok(json!({"caught": "drop-after-denial"}))
}

fn exercise_open_flags() -> Result<(), ProviderError> {
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
        let result = storage::open(&name, options);
        if invalid {
            expect(result, StorageError::InvalidArgument)?;
        } else if create || create_new {
            let file = result.map_err(map)?;
            drop(file);
            if !delete_on_close {
                storage::remove(&name, Durability::Full).map_err(map)?;
            }
        } else {
            expect(result, StorageError::NotFound)?;
        }
    }
    Ok(())
}

fn expect<T>(result: Result<T, StorageError>, expected: StorageError) -> Result<(), ProviderError> {
    match result {
        Err(actual) if actual == expected => Ok(()),
        _ => Err(failure("unexpected-result")),
    }
}
fn map(_error: StorageError) -> ProviderError {
    failure("storage-error")
}
fn failure(code: &str) -> ProviderError {
    ProviderError::new(code, "storage probe failed")
}

dekopon_provider_sdk::export_provider_with_commands!(StorageProbe, bindings);
