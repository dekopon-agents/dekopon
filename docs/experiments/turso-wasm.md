# Turso 0.7.2 `wasm32-unknown-unknown` import gate

**Status: failed early gate; no SQLite feature ships.** Run on 2026-08-20 with `rustc 1.97.0`,
`cargo 1.97.0`, target `wasm32-unknown-unknown`, and `wasm-tools 1.236.1`. The experiment lived in
`/tmp/dekopon-turso-0.7.2-spike`, outside every production manifest.

## Exact package

The first isolated manifest used exactly:

```toml
[dependencies]
turso = "=0.7.2"
```

The crates.io lock entries were:

```text
turso 0.7.2
checksum = f9491d7a80312c5abe66a4409e4dce02065503a235453c94b9e4133877e39ffc

turso_core 0.7.2
checksum = 7a833cc3bf8d4e6c101c504fa470f8ab4270c2202ff2591b61b2e373b4f20d9b
```

The full target tree was captured with:

```console
cargo tree --target wasm32-unknown-unknown -e all > cargo-tree-wasm32.txt
```

Its SHA-256 was
`500fd0724e97bd357f4d25ebdd7ad33e5d514dbbad264447c1b755a5ee2fb7df`.
A later minimal/custom-backend tree had SHA-256
`17b42007df3a55863dad06b70bb3a36323d3770f5877db89c467a1531afd037c`.

## Published API evidence

The published source does have useful engine seams:

- `turso::Builder::with_io_impl(Arc<dyn turso_core::IO>)`;
- `IO::open_file`, `remove_file`, clocks, random number/fill, completion cancellation/draining;
- `File::pread`, `pwrite`, `pwritev`, `size`, `truncate`, and `sync`.

That is not enough for this component gate. In particular, the published `File` lock surface is
`lock_file(exclusive: bool)` / `unlock_file()`, not proof of the complete five-level rollback lock
trace required by Dekopon's durable-files contract.

## Build and dependency results

The unmodified exact dependency failed:

```console
/usr/bin/time -p cargo check --locked --target wasm32-unknown-unknown
# exit 101, real 1.73 s
```

`getrandom 0.2.17` emitted its unsupported `wasm*-unknown-unknown` compile error. The lock also
contained `getrandom 0.3.4` and `0.4.3`.

A second attempt set `turso`'s own `default-features = false`, enabled getrandom 0.2's published
`custom` registration seam, selected the `getrandom_backend="custom"` cfg used by 0.3/0.4, and
provided success-only custom symbols. This proved the custom entropy seams could be selected; it
did **not** make the dependency graph acceptable. After explicitly selecting uuid's non-JS
getrandom source so the next blocker was observable, compilation failed in
`tracing-appender 0.2.5`: its `symlink` dependency exposes neither `symlink_file` nor
`remove_symlink_file` on `wasm32-unknown-unknown` (exit 101, real 17.87 s).

The target tree independently triggered immediate rejection before that compiler error:

```text
wasm-bindgen 0.2.127 <- chrono 0.4.45 <- turso_core 0.7.2
js-sys 0.3.104      <- chrono 0.4.45 <- turso_core 0.7.2
cc 1.4.3 (build)    <- aegis 0.9.15  <- turso_core 0.7.2
bindgen 0.69.5      <- turso_sdk_kit / turso_sync_sdk_kit 0.7.2 <- turso 0.7.2
```

Disabling `turso` defaults did not remove those paths because the published SDK-kit dependencies
still activate `turso_core` defaults. The source also retains filesystem/environment code in core,
although a custom IO could avoid some runtime paths.

## Imports, component size, and reopen

No core Wasm was produced, so there is no truthful core-import list. No component could be
componentized, so there is no final-component import list or size. CRUD, drop-store/fresh
instantiate, broker restart/reopen, `journal_mode=DELETE`, short-read zero-fill, fault injection,
quota failure, fuel, peak memory, and startup-compilation gates were not reached.

This is an intentional gate result, not an invitation to substitute C SQLite, browser SQLite,
libSQL, `rusqlite`, `wasm-bindgen`, or WASI. Production manifests, the root lockfile, configuration,
provider artifacts, and changelog contain no Turso/SQLite dependency, backend, feature, or shipping
claim. Dekopon ships the independently useful JSONL backend and engine-neutral durable-files
contract instead; the latter makes no WAL/SHM or SQLite-compatibility claim.
