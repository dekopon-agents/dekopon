# dekopon-provider-sdk-testkit

Version 0.42.0 includes component asset fixtures and readback. Keep exact core crate
pins at 0.42.0 together; this testkit requires the matching broker-host APIs.

The typed test kit runs a provider against the same SDK dispatch in two modes:
`Native<P>` with injected ports and `Harness::<P>::get(path)` against a checked
WebAssembly component loaded by the real broker host. `conformance::<P>(path)`
checks the component manifest, help and usage, decoded imports against the
provider's declared `Needs`, and closed input schemas. `Run` can script a
loopback HTTPS origin, inject a guest wall clock and narrow host limits. It can also
close the real stdout reader after a bounded prefix (`close_stdout_after(0)`
closes it before invocation) to assert early producer exit without buffering
the rest of the stream.
`Run::settings(value)` and `Native::settings(value)` supply the provider's
settings JSON in place of `providerSettings.<id>`.

`Native<P>` supplies a fixed monotonic reading (zero by default) and deterministic entropy (0xa5 bytes by default); `.monotonic(nanos)` and `.entropy(bytes)` override them for native tests only. Scripted entropy is consumed once; reading past its end panics. The real-component harness always uses the broker's actual invocation clock and OS entropy.

`Run::child(ChildScript { script, status, stdout, stderr, runs_for })` scripts
one expected child per call, in order. Unexpected scripts are typed
`HarnessError::Fixture` refusals for components; native fixture mistakes panic.
`ComponentOutput.children` records each child script and the first MiB of its
stdin as `ChildInput::None`, `Inherit`, or `Reader`. `Native::child` scripts the
same native `Port::spawn` path and `Native::children` returns the captured
runs. A scripted delay advances the native monotonic clock and runs on the
real host for a component.

Storage and external components have no native typed provider; their fixture
integration tests live in `dekopon-broker-host/tests/fixture_host.rs` and use
exact authorized grants against the real storage host. Neither harness grants
production authority or evaluates policy.

Opt in to isolated temporary asset storage and an exact per-call grant with
`Run::assets(AssetConstraints { attach: true, ..Default::default() })`:

```rust,ignore
let run = Harness::<MyProvider>::get(component)
    .assets(AssetConstraints { attach: true, ..Default::default() })
    .http(script);
let origin = run.origin().unwrap().to_owned();
let output = run
    .settings(serde_json::json!({"baseUrl": origin}))
    .call("my-provider.generate", input)?;
let asset = &output.assets.attached[0];
let file = output.assets.files[asset.descriptor as usize].file();
```

`AssetConstraints` is re-exported by the testkit. `output.assets` contains the
broker's asset metadata and owned, already-unlinked files; use positional reads
on `file` to check bytes. Files remain readable until their last owner drops.
The temporary directory is removed when the call finishes, including on failure;
compiled component caching never retains a test's directory or grants. The
existing broker ceilings apply, with a 40 MiB temporary storage budget per call.
Without either asset builder method, asset storage remains absent; without
`.assets(...)`, attachment, removal and send grants remain absent. Storage alone
(`AssetConstraints::default()`) does not allow attachment, removal or sending.
The HTTPS script still grants only its exact origin, method and one request.

Seed input bytes with `Run::asset(id, content_type, bytes)` and reference them in
invocation JSON as `chat-asset:<id>`:

```rust,ignore
let run = Harness::<MyProvider>::get(component)
    .asset(1, "image/png", image_bytes)
    .asset(2, "image/png", mask_bytes)
    .assets(AssetConstraints { attach: true, ..Default::default() })
    .http(script);
let origin = run.origin().unwrap().to_owned();
let output = run.settings(serde_json::json!({"baseUrl": origin}))
    .call("my-provider.edit", serde_json::json!({
        "image": "chat-asset:1", "mask": "chat-asset:2"
    }))?;
let request = output.http_request.unwrap();
```

Input fixtures use identity encoding and broker-owned, read-only, unlinked files
in the same per-call storage budget. Only references in the invocation JSON
receive descriptors, in the broker's own reference order; duplicate fixture IDs,
missing descriptors and byte limits are still checked by broker admission.
An input fixture enables temporary storage and reads, but grants no attachment,
removal or sending. IDs and bytes are private to the call. `http_request` captures
the complete scripted HTTPS request (absolute URI, method, headers and body),
bounded by the existing 64 KiB HTTP grant; it is `None` when no request arrives.

The checked HTTP probe tests synthetic one-pixel PNG inputs through real
`Assets::open` and streamed base64 request parts, captures the sent bytes and
reads the attached response file. This proves the Harness asset/HTTP path, not
GPT-Image's multipart/JSON construction, image validation or response parsing;
those provider-specific generation and edit tests belong in its follow-up.
Delivery sinks are not supplied, so send/delivery tests still need the
broker-host fixture path. `Native` continues to refuse streamed HTTP.
