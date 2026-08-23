# Turso on `wasm32-unknown-unknown`

**Status: shipping, out of this tree.** The
[`turso-sql` provider](https://github.com/dekopon-agents/dekopon-provider-turso-sql) builds
`turso_core` for `wasm32-unknown-unknown` from the
[`dekopon-agents/turso`](https://github.com/dekopon-agents/turso) fork and componentizes it. The
component imports fourteen functions, every one of them `dekopon:storage/durable-files@0.1.0`. No
WASI, no wasm-bindgen, no C.

It has its own repository because the component is 11 MB — larger than this repository's entire
packfile — and its dependency graph is an order of magnitude heavier than any provider example
here. Nothing in this tree depends on it. The gate below is kept because it is this repository's own
experiment record, and because a negative result that was scoped wrong is worth keeping visible.

This supersedes but does not retract the gate below. That run tested the crates.io `turso` wrapper
crate under a no-fork constraint, and under that constraint its result was correct. Every command
output it recorded reproduces exactly. What it got wrong was the scope of the conclusion: the
wrapper is not the engine.

## What the 2026-08-20 crates.io-only gate found

*Retained verbatim. Run with `rustc 1.97.0`, `cargo 1.97.0`, target `wasm32-unknown-unknown`, and
`wasm-tools 1.236.1`, in `/tmp/dekopon-turso-0.7.2-spike`, outside every production manifest.*

The first isolated manifest used exactly `turso = "=0.7.2"` (`turso` checksum
`f9491d7a80312c5abe66a4409e4dce02065503a235453c94b9e4133877e39ffc`, `turso_core`
`7a833cc3bf8d4e6c101c504fa470f8ab4270c2202ff2591b61b2e373b4f20d9b`). The full target tree captured
with `cargo tree --target wasm32-unknown-unknown -e all` had SHA-256
`500fd0724e97bd357f4d25ebdd7ad33e5d514dbbad264447c1b755a5ee2fb7df`; a later minimal/custom-backend
tree had `17b42007df3a55863dad06b70bb3a36323d3770f5877db89c467a1531afd037c`.

The unmodified exact dependency failed at `cargo check --locked --target wasm32-unknown-unknown`
with exit 101 in 1.73 s: `getrandom 0.2.17` emitted its unsupported `wasm*-unknown-unknown` compile
error, and the lock also contained `getrandom 0.3.4` and `0.4.3`. A second attempt disabled `turso`
defaults, enabled getrandom 0.2's `custom` seam, selected the `getrandom_backend="custom"` cfg used
by 0.3/0.4, and supplied success-only custom symbols. That proved the entropy seams could be
selected but did not make the graph acceptable: compilation then failed in `tracing-appender 0.2.5`,
whose `symlink` dependency exposes neither `symlink_file` nor `remove_symlink_file` on this target
(exit 101, 17.87 s). The target tree independently showed:

```text
wasm-bindgen 0.2.127 <- chrono 0.4.45 <- turso_core 0.7.2
js-sys 0.3.104      <- chrono 0.4.45 <- turso_core 0.7.2
cc 1.4.3 (build)    <- aegis 0.9.15  <- turso_core 0.7.2
bindgen 0.69.5      <- turso_sdk_kit / turso_sync_sdk_kit 0.7.2 <- turso 0.7.2
```

No core Wasm was produced, so the run reported no import list, no component, and no CRUD, restart,
fault-injection, quota, fuel, or memory results.

## Where that reasoning went wrong

**The wrapper is not the engine, and the stated justification inverts the dependency direction.**
The gate concluded that "the published SDK-kit dependencies still activate `turso_core` defaults."
`turso_sdk_kit` depends on `turso_core`, not the reverse — the wrapper is a facade over
`turso_sdk_kit::rsapi::TursoDatabase`, which is why no feature flag can de-SDK-kit it and why the
observation is true while the inference is not. Depending on `turso_core` directly removes `bindgen`
and `tracing-appender` from the lockfile outright. Two of the four headline blockers cost nothing.

**The chrono blocker was misattributed.** `turso_core`'s own spec is
`chrono = { default-features = false, features = ["clock"] }`, which pulls neither wasm-bindgen nor
js-sys. The activator was `extensions/core/Cargo.toml:19` — `chrono = { workspace = true,
default-features = true }` — because chrono's `default` includes `wasmbind`, a separate feature from
`clock`. One word on one line of a sibling manifest.

**The aegis finding is real and was under-described.** `cc` is an ungated `[build-dependencies]` of
`aegis`, so no downstream feature removes it from the graph — "one feature flag away" would have
been wrong too. But no C compiler is ever invoked on wasm32; what aegis does by default is link a
prebuilt clang-built `libaegis.a` into the module. Enabling `pure-rust` removes the C from the
artifact, which is the property that matters. The fork extends upstream's existing
Android/macOS pure-rust cfg to `target_family = "wasm"`.

`cc` therefore stays in both the graph and the lockfile no matter what, and no dependency-level
gate on it can ever pass — in the lockfile it also arrives via `loom`, `shuttle`, and
`iana-time-zone-haiku`, which are target- and cfg-agnostic entries. The meaningful gate is the
artifact: `wasm-tools metadata show` on the built component lists `rustc`, `wit-component`, and
`wit-bindgen-rust` and no `clang`, which is what the shipped component shows.

**The lock-surface objection invoked a requirement that does not exist.** The gate held that turso's
`lock_file(exclusive: bool)` / `unlock_file()` was "not proof of the complete five-level rollback
lock trace required by Dekopon's durable-files contract." No such requirement exists. No
`durable-files` I/O path consults handle lock state, `turso_core` never calls `File::lock_file` at
all, and `lock`, `unlock`, and `check-reserved-lock` are called zero times across every invocation.
A coarser guest lock surface is not a compatibility failure.

**`journal_mode=DELETE` was listed as an ungated gate; it is not gateable.** Turso is WAL-only and
upstream has stated it does not plan to add a rollback journal. `PRAGMA journal_mode = DELETE`
returns `wal` — a silent no-op, not a failure.

**Shared memory never enters the picture.** Turso's shared-memory WAL backend is gated behind a
`cfg` requiring a 64-bit Unix or Windows host, so on `wasm32-unknown-unknown` it is not compiled and
the in-process backend — whose WAL index is a heap hashmap — is selected unconditionally. The
durable-files contract's lack of an SHM operation costs nothing here.

## The fork

`dekopon-agents/turso`, branch `dekopon`, based on `b221536e6` (v0.8.0-pre.6). Nine files. Most of
it is upstreamable and is being offered upstream: the `extensions/core` chrono feature, moving
`tempfile` behind the existing `cfg(not(target_family = "wasm"))` table, extending the pure-rust
aegis cfg, guarding `IO::sleep`'s `std::thread::sleep` default, and replacing the remaining
`SystemTime::now`/`Utc::now`/`Local::now` reads that panic "time not implemented on this platform"
in the datetime builtins, `uuid7`, the `time` feature, and `PRAGMA encoding`.

Fork-only: an `extern "Rust"` clock seam on wasm32 that the embedder defines over its host imports,
and selecting the `custom` getrandom backends rather than the browser `js`/`wasm_js` ones. The
latter is not additive — getrandom 0.2 tests `js` before `custom` — so it replaces upstream's
browser choices rather than adding to them, which is why it stays in the fork.

## What still constrains the result

The component is roughly 11 MB and costs several seconds of Cranelift compilation per broker start,
with no cross-process cache. `opt-level="z"` with fat LTO and `panic=abort` takes it to about 7 MB;
`Component::serialize`/`deserialize` would remove Cranelift from the startup path entirely. Neither
is done yet.

Full-text search is absent — `tantivy` is `cfg(not(target_family = "wasm"))`-gated upstream, so
`MATCH` and the `fts` module do not exist on this target. A provider needing search maintains its
own inverted-index table.

The value case is reads, not writes. The storage host materializes a whole file on the first write
of an invocation, so SQLite's usual write-path advantage — touch three pages instead of rewriting
the file — does not exist here. That is a property of the invocation overlay and applies equally to
the JSONL backend, so it argues for neither engine. What SQL actually buys is indexed point queries,
aggregation, and schema.

`parking_lot_core`'s wasm thread parker panics "Parking not supported on this platform" and no
feature removes it. In a single-threaded guest it converts a would-be deadlock into a loud panic,
which is the better failure, but it is a live panic path in shipped code.

The write-ahead log must be truncated before an invocation ends or the namespace becomes
permanently unreadable. That mechanism, and the two other easy-to-lose properties of the adapter,
are documented in [the provider's README](https://github.com/dekopon-agents/dekopon-provider-turso-sql#three-things-that-will-bite-a-modification).
