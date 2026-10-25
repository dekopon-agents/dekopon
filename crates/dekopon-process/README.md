# dekopon-process

`dekopon-process` provides the cancel latch, `CancelHandle` and `CancelSignal`.
`CancelSignal::pair` creates a handle and signal; calling `CancelHandle::cancel`
latches the request permanently, even after every handle is dropped. Both types
are cloneable. Dropping handles without requesting cancellation never cancels;
`CancelSignal::never` never cancels either.

`CancelSignal::is_cancelled` reads the latch synchronously, `watch` exposes its
watch receiver, and the public asynchronous `cancelled()` wait returns as soon as
a request exists. Without a request it stays pending, including after all handles
are dropped.

TreeContext carries the script cancellation signal beside its deadline and shared
budgets. A cancelled tree returns LimitExceeded::Cancelled on its next step charge.
Each job installs its own signal while retaining its starter's shared accounting
and deadline where applicable; killing a child never cancels its starter.

The agent's broker leg drives each command word's broker round trip in place.
Stop or an in-place wait on the tree deadline drops a command word in flight, after which the script
reads `session-cancelled`. A completed round trip is polled first and keeps its
result. The same latch refuses capability calls proposed after Stop before any
proposal exists. The broker remains the only authorizer.

`dekopon-process` owns `TreeContext`, `CallBudget`, `RetainedBytes`, `Limits`,
`LimitExceeded`, and the `DEFAULT_*` limit constants. Shell recursion budgets,
output buffers, pipes, jobs and exit codes remain in `dekopon-shell`.
