# dekopon-tui

The terminal console behind `dekopon console`: pick an agent, see what policy actually grants it,
type a turn, and watch every capability call go out with its arguments and its result.

## Why this runs the loop

`dekopon-brokerd` holds the policy, the provider credentials, and the components. It has no model
client and no concept of a turn — its client-facing surface is eleven operations, of which the two
that matter here are `capabilitiesFor` ("what may this subject do through this agent") and
`invokeFor` ("do this one thing"). Something has to be the gateway half. In production that is
`dekopond`, woken by a chat transport; here it is this crate, woken by somebody typing.

That is not an implementation preference. **Tool-call arguments and results exist only inside the
process running the loop.** `dekopon_agent::prompt::History` keeps the prompt and the answer and
drops every tool call before a turn is recorded. `shell.command` spans deliberately carry an
argument *count* and never argument values. The broker's audit chain carries digests, never
payloads. There is no wire anywhere that carries what this console shows.

## How it observes without changing anything

Every capability an agent reaches passes through one seam,
`CapabilityInvoker::invoke(&self, capability, input, secret_use) -> CapabilityCallResult`, and every
script passes through one more, `ScriptRuntime::run_script`. `RecordingInvoker` and
`RecordingRuntime` wrap those two, forward every method unchanged, and report what went through:

```text
RecordingRuntime
  └ ShellRuntime
      └ RecordingInvoker
          └ SessionInvoker { direct: NoDirect, broker: Arc<BrokerLeg> }
```

Invocation is one method rather than two, which is what makes "forward every method unchanged"
checkable: a wrapper that observes a call cannot drop the secret-use proposal it carries, because
there is no second method to forget.

Neither decorator can influence a session. An observer that could deny a call would be a second
authorization path, and there is exactly one of those. A torn-down console does not stop a running
turn either: a call the broker has already accepted is not something an observer may abort.

`NoDirect` is the local leg, and it is empty on purpose. `dekopon-run` fills that slot with an
import-free Wasmtime registry; the console does not, because every capability it cares about
performs I/O — which the import-free host cannot do — so a local leg would put Wasmtime in the
operator CLI's dependency tree in exchange for nothing.

## Its identity

Sessions propose on behalf of a `dev.<surface>.<name>` subject — `dev.console.xavier`. That service
exists so a development tool does not have to impersonate a real one: a console borrowing
`tel.15550100000` would put a value in `identityMappings`, in Cedar policy, and in the audit chain
that reads like a phone number and is not one.

It is the only subject service nothing authenticated, which is why the broker admits it only under
`allowDevelopmentSubjects: true` and refuses to start if development identities are configured
without it. Declaring one here still grants nothing: the broker resolves it through owner-controlled
`identityMappings` or refuses it, exactly as it does for a subject Slack authenticated.

## What it holds

A model credential. That is the whole list: no policy, no provider credential, no authorization.
Sessions propose on behalf of an attested external subject and the broker decides every one.

The credential file is `chatgpt-auth.console.json` rather than the `chatgpt-auth.json` every other
surface resolves to, because the refresh token rotates: whichever process refreshes invalidates
every other copy, and `dekopond` plus anything seeded from an export of it are all sitting on that
one file. If discovery lands on the shared file anyway — which today only an exported
`DEKOPON_CHATGPT_AUTH_FILE` can do — the console refuses by name and says why. An explicit
`--auth-file` accepts it deliberately, the same shape `dekopon auth chatgpt export` already uses for
`--expose-credential`.

## The shell pane

A line typed there runs on the same `Interpreter`, against the same broker leg, under the same
granted set as a model-authored script — so what is refused there is refused in a turn, for the same
reason. It shows the interpreter's own output rather than the structured call tree, because that
tree is scoped to a turn and a typed line belongs to none. Interpreter state does not survive
between lines, and the pane says so rather than leaving it to be discovered.

## Two views of one conversation

The console keeps a full transcript for display; the model gets `prompt::History`, which keeps only
`{user, answer}` inside a turn and byte window. Turns that have fallen out of that window are drawn
dimmed under a rule. "Why did it forget?" is answerable here and nowhere else.

## Drawing rules

- **No borrowed text reaches a buffer unsanitised.** A pull-request title and an issue body are
  attacker-controlled text arriving through a read-only capability; drawn raw they can move the
  cursor, repaint earlier lines, or reorder a line through a bidirectional override. `redact::sanitize_line`
  is the only way text gets in.
- **Secrets are hidden at render time, per field.** Provider credentials are never in this data by
  construction — the broker injects them inside its native HTTP engine after guest-header
  validation — so what is guarded against is what a *model* wrote or a *provider* returned. A match
  hides only the run that matched, so the sentence around it survives, and revealing is one
  keystroke against one field rather than a mode.
- **`tracing` never reaches stdout.** This process owns the screen; one stray log line corrupts a
  frame.
- **The terminal is restored on every exit path**, including a panic, through a `Drop` guard and a
  panic hook. A panic inside a raw-mode alternate screen otherwise leaves a shell that no longer
  echoes.

## Stopping

`Esc` sets a `CancellationProbe` — the same cooperative stop Slack's Stop button drives. It prevents
the next model turn or tool call from starting. It is not rollback: a provider request the broker
already accepted still finishes, and the console says so rather than claiming the turn was undone.

## License

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your
option.
