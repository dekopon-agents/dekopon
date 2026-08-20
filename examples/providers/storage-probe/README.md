# storage-probe provider

Generated durable-files conformance fixture. It imports exactly
`dekopon:storage/durable-files@0.1.0` and exercises open-flag validation, short positional reads,
sparse/growing writes and truncate, durability modes, two-handle rollback-journal locks,
open-target rename/remove behavior, delete-on-close, entropy, and clocks.

It is intentionally not packaged in any provider directory scanned by default. Regenerate with
`./build.sh`; never edit `../storage-probe-provider.wasm` directly.
