# Mapping a file into guest linear memory

**Status: Exploration — studied, prototyped, and rejected. Nothing here is implemented.** This note
records why, so the next attempt to cut memory on the provider path starts from the measurements
instead of the idea. Every claim is either a source locator or an observation from a prototype.
Wasmtime paths are relative to the published `wasmtime-48.0.2` crate; Dekopon paths were checked at
`v0.17.0`. Re-verify both before relying on them at another version.

## The idea

A provider that receives a large file — an image, an 11 MiB JSON response wrapping one — pays for a
copy of it into guest linear memory. Map the file into the guest instead: the host `mmap`s it,
supplies that mapping through Wasmtime's `MemoryCreator` and `LinearMemory` traits, and the guest
parses it in place with borrowed serde (`&'a str`), no copy.

## Verdict

**Do not adopt.** It can be built, and the prototype built it: a wit-bindgen 0.62.0 component parsed
an 11,184,886-byte envelope with `serde_json::from_slice::<Envelope<'_>>` straight out of a
`MAP_FIXED` window inside its own memory 0, byte-identical to a copying control. Three observations
decide against it.

1. **An unprivileged `ftruncate` kills the privileged broker.** On Linux, Wasmtime installs no
   SIGBUS handler. A read past the new end of a mapped file is not a trap; it is
   `Bus error (core dumped)`, exit 135. A file that arrives from the gateway UID can be truncated by
   the gateway UID at any time.
2. **The guest saves nothing.** Guest linear memory after the parse is 12,386,304 bytes mapped and
   12,386,304 bytes copied. The window lives inside memory 0 and is charged against the same 64 MiB
   cap. The whole gain is host-side: about 11 MB moves from anonymous memory to page cache.
3. **The price is engine-wide.** A component gives the host no handle on its memory, so the host must
   own the allocation through `Config::with_host_memory`. That collides with copy-on-write memory
   images and forces `memory_init_cow(false)` for every provider on the engine.

## What Dekopon configures

| Fact | Locator |
|---|---|
| `wasmtime 48.0.2`, features `component-model, cranelift, parallel-compilation, runtime, std`; no WASI | `Cargo.toml` |
| The whole `Config`: `wasm_component_model(true)`, `consume_fuel(true)` | `crates/dekopon-provider-sdk/src/host.rs` |
| One `Engine`, one `Linker`, a fresh `Store` per operation | `crates/dekopon-broker-host/src/lib.rs` |
| Components load through `Component::deserialize_file` from the cwasm cache | `crates/dekopon-broker-host/src/cwasm.rs` |
| 64 MiB per-memory cap, `DEFAULT_MAX_MEMORY_BYTES` via `StoreLimitsBuilder::memory_size` | `crates/dekopon-provider-sdk/src/host.rs` |

Nothing sets `allocation_strategy`, so allocation is on-demand. Nothing sets `memory_init_cow`, so
copy-on-write images are on. Nothing sets `memory_reservation` or `memory_guard_size`, so on 64-bit
they are 4 GiB and 32 MiB (`wasmtime-environ-48.0.2/src/tunables.rs`); the prototype's creator was
handed exactly those for every memory.

## What the Wasmtime API allows

`Config::with_host_memory(Arc<dyn MemoryCreator>)` (`src/config.rs`) is read in one place,
`Config::build_allocator`, and only on the on-demand arm. **The pooling allocator ignores it
silently.** Adopting a custom creator forecloses pooling.

`MemoryCreator::new_memory(ty, minimum, maximum, reserved_size_in_bytes, guard_size_in_bytes)`
(`src/runtime/memory.rs`, an `unsafe trait`) documents four obligations: reserve
`reserved_size_in_bytes` plus the guard so `grow` never moves the base; leave the guard unmapped,
because JIT code elides bounds checks against it; both sizes are host-page multiples; and **"memory
created from this method should be zero filled."** A creator that returned file-backed pages as the
initial memory would break the last one outright. `LinearMemory` is `byte_size`, `byte_capacity`,
`grow_to`, `as_ptr`.

The base does not move for a wasm32 memory under 64-bit defaults: `memory_may_move` is
`max > reservation` (`wasmtime-environ-48.0.2/src/types.rs`), a 32-bit memory's maximum is at most
4 GiB, and the default reservation is 4 GiB. One hundred grow-by-171-pages iterations kept it fixed.

### The copy-on-write collision

`OnDemandInstanceAllocator::allocate_memory` fetches the module's memory image whether or not a
custom creator is installed (`src/runtime/vm/instance/allocator/on_demand.rs`). `LocalMemory::new`
then requires an mmap-based allocation (`src/runtime/vm/memory.rs`):

```rust
let mmap_base = match alloc.base() {
    MemoryBase::Mmap(offset) => offset,
    MemoryBase::Raw { .. } => {
        unreachable!("memory_image is Some only for mmap-based memories")
    }
};
```

A custom creator's memory is always `Raw` (`src/runtime/trampoline/memory.rs`). So **installing a
`MemoryCreator` without `memory_init_cow(false)` panics at instantiation** for any component whose
data segments qualify for an image:

```text
thread 'main' panicked at wasmtime-48.0.2/src/runtime/vm/memory.rs:569:29:
internal error: entered unreachable code: memory_image is Some only for mmap-based memories
```

Observed on linux/arm64 through both `Component::from_file` and `Component::deserialize_file`, and
on macOS arm64 through `deserialize_file` — which is how Dekopon loads every component. On macOS
`from_file` happens not to panic, because the in-memory image source is Linux-only while the file
image source is not (`src/runtime/vm/sys/unix/vm.rs`); do not read that as safety.

With `memory_init_cow(false)`, instantiation copies data segments and a `Raw` base is fine. That is
the only working combination, and its cost is below.

A landmine for anyone who later switches allocators: a pooled slot is reset with
`madvise(MADV_DONTNEED)` on Linux (`src/runtime/vm/cow.rs`). Against a `MAP_PRIVATE` file window
that restores the **file's bytes**, not zeros — one invocation's asset visible to the next.

## What the component model allows

**A custom creator is called for memories inside a component.** Observed: `new_memory
minimum=1114112 maximum=Some(4294967296) reservation=4294967296 guard=33554432` for memory 0, and
`as_ptr()` was the base the guest's pointers are relative to.

**A component cannot import a memory.** A core module with `(import "env" "memory" (memory 1))` run
through `wasm-tools component new` (1.259.0, the `ci/toolchain.env` pin) comes out with a
synthesized `$wit-component-shim-module` that *defines* the memory and wires it in. The component's
import surface has funcs, values, types, instances and components; no core memory. A Dekopon guest
always defines its own memory 0 — `(memory (;0;) 17)`, exported and aliased for the canonical ABI —
and the host cannot substitute one.

**The host cannot reach the memory a component has.** `wasmtime::component::Instance` exposes
`get_func`, `get_typed_func`, `get_module`, `get_resource`, `get_export`, `get_export_index` and
`instance_pre` (`src/runtime/component/instance.rs`). There is no `get_memory`. Core-module
`Instance::get_memory` → `Memory::data_ptr` would make remap-on-demand work with no custom creator,
but Dekopon runs components. **So the custom `MemoryCreator` is the only supported way to learn a
component's linear-memory base**, and every variant inherits the copy-on-write cost.

Three shapes were considered:

| Variant | Outcome |
|---|---|
| **File as a sub-range of memory 0, supplied by the creator.** | Dominated. The guest still needs a call to learn `(ptr, len)`; the window exists on every invocation, used or not; and bytes the creator adds above the module's declared minimum are **invisible to the `ResourceLimiter`**, because `limit_new` runs on the declared minimum before `new_memory` is called. The allocator is not the problem: Rust's wasm32 dlmalloc takes heap only from `memory.grow` and never returns pages. |
| **Remap on demand over a guest-supplied window.** | Works; this is what the prototype does. The guest takes pages with `core::arch::wasm32::memory_grow(0, pages)` — 64 KiB aligned, never reused by the allocator — and the host does `mmap(base + offset, len, PROT_READ, MAP_PRIVATE \| MAP_FIXED, fd, 0)`. Growth afterwards appends above the window and only `mprotect`s. One `munmap(base, reservation + guard)` in the `LinearMemory` destructor takes down anonymous pages and the file window together. 100 iterations returned to baseline RSS with no leak. |
| **A second linear memory backed by the file.** | Dead twice. The canonical ABI binds one memory per lift and lower (`(canon lift … (memory $memory) (realloc $cabi_realloc))`), and a Rust reference on wasm32 lowers to an `i32` offset into the default memory with no address-space qualifier. |

## Fault safety

Wasmtime's Unix handler covers SIGSEGV, SIGILL, SIGFPE on x86 and s390x — and SIGBUS only on Apple
and FreeBSD (`src/runtime/vm/sys/unix/signals.rs`):

```rust
// Sometimes we need to handle SIGBUS too:
// - On Darwin, guard page accesses are raised as SIGBUS.
if cfg!(target_vendor = "apple") || cfg!(target_os = "freebsd") {
    f(&raw mut PREV_SIGBUS, libc::SIGBUS);
}
```

On Linux the signal takes its default disposition. It never reaches `test_if_trap`, so a program
counter in wasm code and a fault address inside linear memory change nothing.

Observed on linux/arm64, the guest reading its window after the file went from 11,184,886 to 65,536
bytes:

```text
host: ftruncate -> 65536 bytes: Ok(()); file is now 65536 bytes
guest note: truncate-guest: truncate rc=0, now touching tail
Bus error (core dumped)
### PROCESS DIED: exit=135, signal=7 (BUS)
```

- **`MAP_POPULATE` does not help.** Same death: Linux invalidates the mapping's pages on truncate
  whatever their residency.
- **A host-side read dies on every platform.** `test_if_trap` begins with `lookup_code(regs.pc)`; a
  program counter outside compiled wasm is `TrapTest::NotWasm`, and the handler restores the default
  disposition. Observed on linux/arm64 with the host's own scan of the window. Everything the host
  does with guest bytes is that shape: lifting a `list<u8>` or `string` whose pointer lands in the
  window, hashing, the credential echo scan.
- **No mitigation is real.** memfd with `F_SEAL_SHRINK | F_SEAL_WRITE` is immune, but a file from
  another UID is an ordinary file, and getting its bytes into a memfd is the copy being avoided.
  `MAP_PRIVATE` is what the prototype already uses. macOS has no seals. `fstat`-then-map is a race
  by construction.
- **macOS hides the bug.** Mach exception ports turn `EXC_BAD_ACCESS` in JIT code into a clean trap
  (`src/runtime/vm/sys/unix/machports.rs`), and in three runs the fault never occurred at all:
  Darwin kept the pages alive for the existing mapping. A defect of this class would show up only in
  production.
- **Even a broker-owned file is exposed.** A file-backed mapping raises SIGBUS on an I/O error, not
  only past the end. A read error on the Pi's storage is an `io::Error` today; mapped, it is a
  process kill.

A guest write into the `PROT_READ` window is clean on both platforms: a wasm trap. Its message
misleads — `memory fault at wasm address 0x120000 in linear memory of size 0xbd0000 … out of bounds
memory access` for an address well inside the memory, because that is the trap code Cranelift
attached to the instruction.

## What it saves

linux/arm64 container, 4 KiB kernel pages, Rust 1.98.1, custom creator with
`memory_init_cow(false)`, one 11,184,886-byte envelope
`{"data":[{"b64_json":"<8 MiB of seeded base64>"}],"usage":{…}}` parsed with `b64_json: &'a str`.

| | copy in, then parse | `MAP_FIXED` window, then parse |
|---|---:|---:|
| **Guest linear memory after parse** | **12,386,304 B** | **12,386,304 B** |
| `RssAnon` before → after | 11,260 → 22,212 kB (+10,952) | 11,224 → 11,256 kB (+32) |
| `RssFile` before → after | 8,292 → 8,292 kB | 8,292 → 19,216 kB (+10,924) |
| cgroup v2 `memory.current`, 512 MiB limit | 35,471,360 B | 24,489,984 B |
| After `Store` drop | `RssAnon` 11,260 kB | `RssAnon` 11,224 kB |

The host-side gain is real and not the usual page-cache hand-wave: page cache is charged to the
cgroup, so this is 11 MB of charge that becomes reclaimable under pressure instead of anonymous. It
is also all there is. The guest is unchanged, and a host `read` that serves the guest in 64 KiB
chunks already keeps host heap flat without any of this.

Instantiation, 500 runs after 20 warm-ups, fresh `Store` each:

| Configuration | study guest, 17 pages | `file-provider.wasm`, 18 pages |
|---|---:|---:|
| Default: copy-on-write on, Wasmtime memory | 11.176 µs | 10.531 µs |
| `memory_init_cow(false)`, Wasmtime memory | 17.536 µs | 23.460 µs |
| `memory_init_cow(false)` + custom `MemoryCreator` | 25.614 µs | 31.485 µs |

Trivial beside an HTTP call. Listed because it is unavoidable, scales with data-segment size, and
lands on every provider: `Config` is per-`Engine` and the broker has one.

## If the API were ever wanted anyway

A host call cannot return a borrowed view: a `list<u8>` result is always lowered into
`cabi_realloc` memory. The address would travel as an integer and the SDK would build the slice:

```wit
/// Map stored bytes over a guest-owned, 64 KiB-aligned window. `none` unless the source is a
/// file the host can map.
map-into: func(ptr: u32, len: u32) -> result<option<u64>, error>;
```

About fifteen lines of `unsafe` in the SDK around `memory_grow` and `slice::from_raw_parts`, with a
lifetime the type system cannot express — valid until the store drops, unsound if two windows
overlap. It would be an optional property of a file-backed source, refused elsewhere, so it does
not constrain a sequential read interface.

64 KiB wasm pages are a multiple of 4, 16 and 64 KiB host pages, so alignment holds on the Pi and
on macOS (16,384 observed). The last host page past the end of the file reads as zeros.

## Revisit when

1. **Never, for a file another UID can truncate.**
2. **For a broker-owned file,** if Wasmtime gains a host buffer in the canonical ABI or an accessor
   for a component's core memory. The copy-on-write cost then disappears and only the I/O-error
   SIGBUS remains, which `mlock` or a memory-medium volume would close.
3. **For chunked transfer between two live guests,** study what already ships: wasmtime 48 has
   component-model `stream<T>` and `future<T>` behind `Config::concurrency_support`
   (`src/runtime/component/concurrent/futures_and_streams.rs`). Bounded-chunk copies, not zero-copy,
   but the supported route.

## Not established

- The production Pi's kernel page size. Alignment holds for 4, 16 and 64 KiB either way.
- Whether Darwin's no-fault-after-truncate survives a cold buffer cache.
- What 11 MB of reclaimable file cache is worth in the real pod under pressure.

## Rebuilding the prototype

It is not in this tree. A host binary pins `wasmtime = "=48.0.2"` with the workspace's features and
`libc`; a guest component is built with wit-bindgen 0.62.0 and `wasm-tools component new`. The host
installs a `MemoryCreator` that reserves `reservation + guard` as `PROT_NONE` and `mprotect`s the
live prefix, sets `memory_init_cow(false)`, and exports one function that `MAP_FIXED`s a descriptor
over `(ptr, len)`. The guest takes its window with `memory_grow`, calls that function, and parses.
Modes: copy control, mapped, truncate during a guest read, truncate during a host read, guest write
to the window, copy-on-write left on, and a 100-iteration loop. Run it on linux/arm64; macOS will
not show the failure.
