# dekopon-storage-host

Wasmtime-independent broker-owned storage engine for namespace-bound provider imports.

The host derives opaque paths with domain-separated HMAC-SHA-256, retains directory descriptors for
the complete tree, and performs opens, scans, creation, rename, and unlink relative to those
descriptors with no-follow and identity/link checks. It keeps an exclusive root writer lock and a
defined base-then-generation lease order, and rebuilds logical quota accounting on startup.
Each invocation uses a direct namespace/VFS handle. Each authorized write affects the live files
at that host call; provider failure, trap, cancellation, or an invalid response does not undo
completed writes. There is no invocation-wide atomicity, rollback, crash recovery, or automatic
collection of inactive generations. Unknown retained layout entries are refused/quarantined.

Accounting is logical rather than a physical-disk claim: apparent bytes plus 4096 bytes for every
file and directory, including quarantine and authority-pointer replacement temporaries. Namespace creation,
authority-pointer replacement, live-file growth and entry count are reserved before mutation.
The process ledger is rebuilt once at startup and updated by host-owned mutations; unreadable
usage after a partial syscall failure retains conservative headroom. Sparse gaps, growing truncate,
and JSONL's host-added LF consume write/quota budgets. Authority-pointer replacement and entry
operations retain temporary headroom; direct JSONL replacement and positional writes reserve
live-file growth, not a staged copy of the existing file. Metadata-only size/stat calls
do not load a whole file; native reads remain bounded by invocation ceilings.
`maxPendingTransactions` is the compatibility spelling for the bound on concurrently active
invocation handles, not a transaction queue.

## Durable-files contract

### Open flags

| Combination | Result |
|---|---|
| neither `read` nor `write` | `invalid-argument` |
| `create`, `create-new`, or `delete-on-close` without `write` | `invalid-argument` |
| `create` and `create-new` together | `invalid-argument` |
| missing file without either create flag | `not-found` |
| existing file with `create-new` | `already-exists` |
| every other read/write combination | valid, within handle and quota limits |

Reads are positional and return available bytes, including an empty short read at or beyond EOF. A
SQLite adapter must zero-fill its own short-read buffer; this is load-bearing rather than
hypothetical, because turso treats any short read as a hard error and does not zero-fill itself. Positional writes are exact-or-error and
charge both supplied bytes and any sparse logical growth. Remove, replacement, or rename of an open
source or target is `busy`. `delete-on-close` marks the file for unlink and applies it only after the
last invocation handle closes.

A file identity is nonzero, equality-only, and stable for one live logical file. It is not an inode,
path, generation, timestamp, or ordering value.

### Lock table

Promotion is exactly:

```text
none -> shared -> reserved -> pending -> exclusive
```

A skipped or reversed promotion is `invalid-argument`. `unlock(to)` may downgrade to any level no
higher than the handle's current level; drop releases every level. Shared locks coexist. Only one
handle may hold reserved or pending. Pending blocks every new shared reader while existing shared
readers drain. Exclusive requires every other handle on that file to be at `none`. Incompatible handles in the same invocation return `busy` immediately rather than waiting
and deadlocking a single-threaded guest. `check-reserved-lock` observes reserved, pending, or
exclusive on any live handle.

These are rollback-journal primitives, and no I/O path consults them: read, write, size, truncate,
and sync never inspect handle lock state, so a guest may read and write at `none`. The table
constrains the shape of a lock sequence, not access.

The ladder is currently unused. Turso is WAL-only, its own lock surface is two-state, and
`turso_core` never calls `lock_file` at all, so an adapter that never locks is equally correct — do
not read a coarser guest lock surface as a compatibility failure.

There is no SHM operation and no multiprocess-database claim. There is no WAL *implementation*
either, but a single-instance WAL engine needs neither: its log is an ordinary durable file and its
index lives in guest memory. Database and log writes are independent host calls. The host provides no atomic database/log
commit or torn-WAL recovery guarantee; a failed invocation can leave partial database changes.

### Durability

All sync modes synchronize the live file with `sync_all` and its parent directory.
They do not create an invocation commit
point or make a sequence of calls atomic. Applications must not infer crash durability or
cross-file consistency from a successful invocation or sync request.

## Native I/O threat and timing limits

Filesystem calls are native blocking operations. Cancellation/timeout is a signal, not a hard
wall-clock bound on a stuck kernel `fsync`; the broker adapter retains the namespace lease and quota
reservation until every started job drains. The finalization deadline starts before that drain and
is checked before each next bounded filesystem step, while one started step may contain descriptor
validation plus a native operation that outlives it. Operators must size shutdown grace for host
timeout, lease wait, finalization, and framing, while accepting that a failed native filesystem can
still exceed it.

Retained directory descriptors, descriptor-relative no-follow operations, broker-derived opaque
components, mode/owner/link checks, and before/after identity checks refuse ordinary corruption and
unsafe layout. Base leases serialize authority pointers, grants, and invocation access; unique create-new
temporaries cannot unlink one another. These controls do **not** claim protection from an actively
malicious same-UID process racing filesystem mutation. Run the broker under a dedicated UID and
mount boundary when that actor is in scope, and use a supported local filesystem with advisory
locks, same-directory atomic rename, and file/directory synchronization semantics.
