# dekopon-process

`dekopon-process` is Dekopon's small, unprivileged Tokio lifecycle seam for
frontend operations. Its current API runs one asynchronous `Process` as one
payload-free traced Tokio node and joins that task before returning. While the
owning Tokio runtime remains alive, its supervisor also continues joining and
observing the node if the outer `execute` future is dropped. A completed
operation preserves its own typed success or error; a task panic or cancellation
preserves Tokio's `JoinError`.

The first production consumer is `dekopon-run shell`. It executes provider
loading and the existing synchronous interpreter with `spawn_blocking` inside
one opaque `legacy-shell` process. That process is non-interruptible after start and is owned by a
self-contained supervisor task. Dropping the outer `execute` future detaches the
supervisor, not the process node: while the runtime lives, the supervisor still
awaits and records the node. The result travels in an RAII envelope whose drop
invokes the required abandonment observer, including when a queued result is
never polled by the outer future. Runtime shutdown is the ownership boundary;
normal `dekopon-run`
keeps its runtime alive through command completion. Existing shell values,
pipelines, output, status,
limits, and provider behavior remain owned by `dekopon-shell`.

This crate does **not** currently provide structured process trees, scopes,
ports, cooperative cancellation, deadlines, graph scheduling, Bash parsing,
provider loading, authorization, credentials, retries, or persistence. Those
orchestration facilities remain deferred until a real stage-level frontend
consumer defines the smallest useful contract. The broker remains the only
authorization boundary.
