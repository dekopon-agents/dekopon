# Harness gotchas

**Status: Current.** How the gateway's turn loop hands a running turn's mailbox over to the next
turn, and the race that handoff is built to rule out. The turn loop itself is described in
[Sessions](gatewayd.md#sessions); the steering modes are below its numbered steps.

## Vocabulary

- **Staged.** A steer the loop took from the mailbox and put into the transcript before a model
  call. It belongs to the running turn and joins that turn's user text in history.
- **Unstaged.** A steer admitted to the mailbox that the loop has not taken yet.
- **Leftover.** A steer still unstaged when the turn seals. It runs as the next turn, under its own
  sender, ahead of every follow-up already queued.
- **Seal.** The one step that ends a turn's mailbox: the running state becomes completing, so admission
  queues later input as a follow-up instead of a steer. A stop and a seal race on one
  compare-and-swap; exactly one wins.

## The invariant

Every steer is staged into its turn or returned as a leftover that runs next.

## The race the seal rules out

Admission decides between steer and follow-up by reading the turn's state under the gateway's
`in_flight` lock. If the loop checked for steers and then, as a separate step, marked the turn
complete, a steer admitted between the two would see a running turn, enter the mailbox, and never be
read: a check-then-act race, the lost-wakeup shape. There are two ways in:

- **The check-to-claim gap.** The model answers with steps left, the loop finds no steers, and a
  steer arrives before the turn is marked complete.
- **The step cap.** On the last step the loop stops before it would read the mailbox again, so a
  steer admitted while that step's tools ran is never read by this turn.

`run_turns` owns a `MailboxLease` by value, so every exit passes through it:

- `take()` stages steers before each model call.
- `seal_or_take()` runs at a final answer with steps left. Under the `in_flight` lock it either
  returns the waiting steers, and the loop keeps working, or seals with none waiting. No steer can
  arrive between the check and the seal.
- `seal()` closes every other exit: the step cap, an error, a stop, and the lease's `Drop` on a
  panic. It returns `Sealed { leftover }` or `Stopped`. Leftovers stay in the mailbox and the handoff
  moves them to the front of the follow-ups in arrival order.

The seal is idempotent, so the gateway seals again after the blocking task returns. That covers a
loop that never built a lease, and a stop that wins there makes the turn stopped, not answered.

## Folding

A leftover run folds into one turn under the leftover rule: a native message or wake leads, and the
native messages directly behind it join it. At handoff, consecutive native follow-ups from the
same sender fold the same way; another sender, a job or a wake ends the run. Different senders never
fold. A folded steer carries every native message id it absorbed, and all of them count as consumed,
so later platform history never replays its text.
