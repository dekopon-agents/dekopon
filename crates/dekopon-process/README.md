# dekopon-process

`dekopon-process` is Dekopon's small, unprivileged Tokio lifecycle seam for frontend operations.
It runs one asynchronous `Process` as one payload-free traced Tokio node and joins that task
before returning. The node lives in a one-task `JoinSet` owned by the `execute` future, so
dropping that future aborts it. A completed operation preserves its own typed success or error; a
task panic or cancellation preserves Tokio's `JoinError`.

A process is either non-interruptible or cancellable. A cancellable process is built from a
`CancelSignal`; its paired `CancelHandle` requests cancellation. Cancellation is cooperative and
minimal: `execute` aborts the node's Tokio task, which lands at the node's next `.await`, and
then joins that task before reporting `ProcessOutcome::TaskFailed` with `is_cancelled()`. It never
returns while the node's own task could be running, a node that returned before the abort landed
keeps its real result, and dropping every handle never cancels (`CancelSignal::never` is a signal
nobody can request). `execute` joins the node's own Tokio task and nothing else: work the node
handed to `spawn_blocking` or spawned as another task is detached by the abort, is not joined, and
can outlive a `cancelled` outcome. A node that must not leave such work behind must stay
`non_interruptible`. The node span records `process.interruptibility` as `non-interruptible` or
`cancellable` and a requested cancellation as `process.outcome = "cancelled"`, distinct from the
`task-cancelled` a runtime-driven abort records.

`dekopon-agent`'s broker leg runs each command word as one cancellable `broker-command` node
around the broker round trip. `dekopond` ties its `CancelSignal` to the session's Stop, so a run in
flight is aborted at its next await and joined before the script reads `session-cancelled`.
`CancelSignal::is_cancelled` is the same request read synchronously, for a caller deciding whether
to start work rather than awaiting the end of work already running; the leg takes it before
proposing a capability call at all, so a stopped session opens no further round trip.
Embedders may instead supply `CancelSignal::never`. The leg drives `execute` to completion with
`block_on`, so the future is never dropped early. Shell values, pipelines, output, status, and
limits belong to `dekopon-shell`.

The crate provides no structured process trees, scopes, ports, deadlines, graph scheduling, Bash
parsing, provider loading, authorization, or credentials, and its cancellation is the
abort-then-join contract above: no deadline, no propagation to child work, no way to interrupt a
blocking thread. Retries and persistence are
[constitution non-goals](../../docs/design.md#non-goals). The broker is the only authorization
boundary.
