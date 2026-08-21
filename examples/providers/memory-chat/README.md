# memory-chat provider

Generated broker-only JSONL provider implementing hidden `memory.chat.record` plus visible
`memory.chat.recent` and `memory.chat.search`. It imports exactly
`dekopon:storage/jsonl@0.1.0`, never HTTP, durable-files, WASI, or ambient process I/O.

`turns.jsonl` holds versioned turns and may compact with hysteresis. `dedup.jsonl` holds permanent
finite id/commitment entries and is never compacted; reaching the broker-configured record/byte cap
returns `dedup-capacity` while reads remain available. Search is Unicode-lowercased literal
substring matching over the bounded newest lookback. Memory is retrieved only on demand.

Regenerate with `./build.sh`; never edit `../memory-chat-provider.wasm` directly.
