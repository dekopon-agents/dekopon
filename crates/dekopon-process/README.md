# dekopon-process

`dekopon-process` is Dekopon's small, unprivileged Tokio lifecycle seam for
frontend operations. Its current API runs one asynchronous `Process` as one
payload-free traced Tokio node and joins that task before returning. While the
owning Tokio runtime remains alive, its supervisor also continues joining and
observing the node if the outer `execute` future is dropped. A completed
operation preserves its own typed success or error; a task panic or cancellation
preserves Tokio's `JoinError`.

A process is either non-interruptible or cancellable. A cancellable process is
built from a `CancelSignal`; its paired `CancelHandle` requests cancellation.
Cancellation is cooperative and minimal: the supervisor aborts the node's Tokio
task, which lands at the node's next `.await`, and then still joins that task
before reporting `ProcessOutcome::TaskFailed` with `is_cancelled()`. It never
returns while the node's own task could still be running, a node that returned
before the abort landed keeps its real result, and dropping every handle never
cancels (`CancelSignal::never` is a signal nobody can request). The supervisor
joins the node's own Tokio task and nothing else: work the node handed to
`spawn_blocking` or spawned as another task is detached by the abort, is not
joined, and can outlive a `cancelled` outcome. A node that must not leave such
work behind must stay `non_interruptible`, as the runner's `legacy-shell` and
`direct-command` nodes do. The node span records
`process.interruptibility` as `non-interruptible` or `cancellable` and a
requested cancellation as `process.outcome = "cancelled"`, distinct from the
`task-cancelled` a runtime-driven abort records.

Three consumers exist. `dekopon-run shell` executes provider loading and the
existing synchronous interpreter with `spawn_blocking` inside one opaque
`legacy-shell` process, non-interruptible after start and owned by a
self-contained supervisor task. `dekopon-run`'s direct leg runs each provider
command word a script executes as one `direct-command` node, non-interruptible
for the reason above: the guest call is `spawn_blocking` work the supervisor
would not join. `dekopon-agent`'s broker leg runs each command word as one
cancellable `broker-command` node around the broker round trip; `dekopond` ties
its `CancelSignal` to the session's Stop, so a run in flight is aborted at its
next await and joined before the script reads `session-cancelled`, while
`dekopon-run prompt --broker` supplies `CancelSignal::never`. Dropping the outer `execute` future detaches the
supervisor, not the process node: while the runtime lives, the supervisor still
awaits and records the node. The result travels in an RAII envelope whose drop
invokes the required abandonment observer, including when a queued result is
never polled by the outer future. Runtime shutdown is the ownership boundary;
normal `dekopon-run`
keeps its runtime alive through command completion. Existing shell values,
pipelines, output, status,
limits, and provider behavior remain owned by `dekopon-shell`.

This crate does **not** currently provide structured process trees, scopes,
ports, deadlines, graph scheduling, Bash parsing, provider loading,
authorization, credentials, retries, or persistence, and its cancellation is
only the abort-then-join contract above: no deadline, no propagation to child
work, and no way to interrupt a blocking thread. Those
orchestration facilities remain deferred until a real stage-level frontend
consumer defines the smallest useful contract. The broker remains the only
authorization boundary.
