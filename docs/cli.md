# CLI reference

`dekopon` is the operator interface for local catalog inspection, model-account lifecycle, and the interactive console. Catalog commands are synchronous, read a validated local catalog, and contact no other process; `auth` commands instead contact the selected model provider's fixed authentication endpoint. `console` is the one command that reaches another process: it is an unprivileged `dekopon-brokerd` client that runs an agent session locally, and it is documented in [its own section](#the-interactive-console) below. `dekopond` remains uncontacted by every command here. The separate experimental `dekopon-run` executable loads read-only Wasm providers and is documented in [`run.md`](run.md). `dekopon-brokerd provider` is a distinct offline deployment-lifecycle surface rather than part of this catalog CLI; its exact-reference sync/list/verify contract is in the [broker operations manual](../crates/dekopon-brokerd/README.md#managed-provider-sets), alongside the equally offline `dekopon-brokerd audit verify --audit-path <PATH>`, which checks a durable audit chain without starting the broker.

## Commands

```text
dekopon version
dekopon auth chatgpt login
dekopon auth chatgpt status
dekopon auth chatgpt logout
dekopon auth chatgpt export --expose-credential
dekopon get agents
dekopon get agent <NAME>
dekopon get capabilities
dekopon get capability <NAME>
dekopon get providers
dekopon get provider <NAME>
dekopon describe agent <NAME>
dekopon validate
dekopon config view
dekopon console [--subject <SUBJECT>] [--socket <PATH>] [--server-uid <UID>]
                [--model <MODEL>] [--auth-file <PATH> | --endpoint <URL> [--api-key-env <NAME>]]
                [--max-steps <COUNT>] [--max-capability-calls <COUNT>]
```

`dekopon` with no subcommand opens the console when standard input and standard output are both
terminals. When either is not — a pipe, a redirect, a CI step — it remains the usage error it has
always been, printing help to standard error and exiting `2`. Both halves of that check are
required: drawing needs a terminal to draw on, and the console needs one to read a key from, so a
piped invocation that opened a full-screen console would hang forever on input that never arrives.

Run `dekopon --help` or `dekopon <COMMAND> --help` for generated syntax.

## Global flags

- `--config <PATH>`: authoritative YAML or JSON source for catalog commands; ignored by `version` and `auth`.
- `-o, --output <FORMAT>`: `table` (default), `wide`, `json`, `yaml`, or `name`.
- `--no-color`: disable ANSI color in diagnostics.
- `--quiet`: suppress successful output; errors still print.
- `-v`: emit informational diagnostics and error causes.
- `-vv`: emit debug diagnostics and debug error context.

Global flags may appear before or after subcommands. Authentication commands do not load the catalog. `--output json` or `--output yaml` keeps authentication status machine-readable; device-login instructions are written to standard error so standard output remains parseable. `--output` does not apply to `auth chatgpt export`, whose form is chosen by `--format`.

## ChatGPT subscription authentication

`dekopon auth chatgpt login` uses OpenAI's Codex device authorization flow and writes only to Dekopon's credential file. `status` reports state without revealing tokens, and `logout` removes only Dekopon's file. The default is `~/.config/dekopon/chatgpt-auth.json`; override it with `DEKOPON_CHATGPT_AUTH_FILE` or `--auth-file <PATH>`. Discovery treats a variable exported with an empty value as unset and falls through to the next tier, and refuses a discovered path that is not absolute — a relative `DEKOPON_CHATGPT_AUTH_FILE` or `XDG_CONFIG_HOME` would otherwise leave the rotating refresh token in whatever directory the process started in. Only `--auth-file` is taken verbatim. See [`run.md`](run.md) for inference behavior and the complete security boundary.

### Exporting a credential for a secret store

`dekopon auth chatgpt export` prints an existing local credential so it can be seeded into a secret store. It exists because device authorization needs a human at a browser: a pod can only ever run on a credential an operator carried out of a local login. It resolves the credential file exactly as `login`, `status`, and `logout` do, including `--auth-file`.

**This is the one Dekopon command whose output is credential material in the clear.** Everywhere else a credential renders a redaction marker. Two gates and a warning make that deliberate rather than incidental:

- `--expose-credential` is required. It has no default and no short form, so exporting is something an operator typed, and it is greppable in a shell history or a runbook.
- Standard output is refused when it is a terminal, because intent does not cover destination: an operator who means to export still should not leave a live refresh token in scrollback, a `tmux` capture, or a screen share. Every intended consumer is a pipe or a redirect. `--allow-terminal` overrides it.
- Both forms warn on standard error that the copy is stale the moment the live credential refreshes, and the Secret manifest repeats that in a comment header, because the manifest outlives the terminal.

| Flag | Meaning |
|---|---|
| `--format secret` | Default. A `v1` `Secret` manifest for `kubectl apply -f -`. |
| `--format raw` | The credential document itself, byte-identical to what a login writes, for a password-manager field. |
| `--secret-name <NAME>` | Secret name, default `dekopon-chatgpt-auth`; validated as an RFC 1123 subdomain before the credential is read. |
| `--namespace <NAMESPACE>` | Secret namespace; omitted from the manifest when unset. |
| `--expose-credential` | Required acknowledgement that this prints a live access token and refresh token. |
| `--allow-terminal` | Print to a terminal anyway. |

`--quiet` is refused, because suppressing the document while exiting `0` is how a scripted seeding step stores nothing and believes it succeeded.

The manifest carries the document under the key `chatgpt-auth.json`, matching Dekopon's own file name. Missing, malformed, incomplete, and unsupported-version credential files all fail with exit code `1` and print nothing, so a seeding step never stores a half-formed secret.

The refresh token rotates, so an exported copy is invalidated by the next refresh of the credential it came from. [`chatgpt-credential.md`](chatgpt-credential.md) is the full deployment lifecycle: export, store, seed once into a writable directory, and re-export only on a deliberate rotation.

## The interactive console

`dekopon console` opens a full-screen view over a running `dekopon-brokerd`. It is an ordinary
unprivileged client of that broker's Unix socket: it holds a model credential and nothing else — no
policy, no provider credential, no authorization — and every capability call it makes is a proposal
the broker alone decides.

**It runs the agent session itself.** The broker has no model client and no concept of a turn; in a
deployment `dekopond` runs the loop and the broker authorizes each capability the loop reaches. The
console takes that gateway role for one operator at one terminal. This is what lets it show tool
arguments and results at all: `dekopon-agent`'s conversation history keeps only the prompt and the
answer, `shell.command` spans carry an argument count rather than argument values, and audit records
carry digests rather than payloads, so those values exist only inside the process running the loop.

Four panes, cycled with `Tab`: the catalog's agents; one agent's declared capabilities beside what
policy actually grants it; the conversation with each turn's scripts and capability calls; and a
prompt bound to the same capability seam the agent's own sessions dispatch through. `?` lists the
keys. `Esc` requests a cooperative stop, which prevents further work and does not undo a call the
broker already accepted.

### Connecting

The socket resolves as [`run.md`](run.md) documents: `--socket`, then `$DEKOPON_BROKER_SOCKET`, then
`$XDG_RUNTIME_DIR/dekopon/broker.sock`, then `$HOME/.local/run/dekopon/broker.sock`. Candidates are
never probed for existence, because an absent socket is what a stopped daemon looks like; the
tightest resolved tier is trusted, and a daemon that is not running surfaces as
`no broker found at <path>` naming that exact path and the tier it came from. `--server-uid`
defaults to the caller's own effective UID.

### Identity

`--subject <SUBJECT>` (or `DEKOPON_CONSOLE_SUBJECT`) is the canonical external subject sessions
propose on behalf of, normally `dev.console.<name>`. There is no default and none is derived: an
identity the console guessed is an identity nobody chose, and the broker would refuse it one step
later having explained nothing.

`dev` is a subject service like `slack` or `tel`, with one difference that decides how it is
treated: nothing authenticated it. The others carry a name a real service verified before the
message reached a transport; a `dev.*` subject carries a name a local caller typed on an owner-only
socket. So a broker admits one only under `allowDevelopmentSubjects: true`, and refuses to start if
its configuration names development identities without it. The alternative — a console borrowing
`tel.15550100000` — would put a value in `identityMappings`, in Cedar policy, and in the audit
chain that reads like a phone number and is not one.

The `console` segment names the surface that minted the subject, so a grant can admit
`dev.console` without also admitting `dev.ci`.

Declaring a subject grants nothing. Entering an agent opens an attested leg, so what the console may
do is what policy grants *that subject through that agent* — which requires `allowDevelopmentSubjects`,
an `attestor.namespaces` entry covering the subject's namespace, and an `identityMappings` entry
resolving it to a principal, all in the broker's own owner-only configuration. A subject the broker will not resolve produces an
empty capability surface, which the console reports as policy granting nothing rather than as an
unreachable broker.

### The model credential

The console resolves `chatgpt-auth.console.json`, **not** the `chatgpt-auth.json` that `auth chatgpt`
and a gateway `authFile` resolve to. Precedence is otherwise identical: `--auth-file`, then
`$DEKOPON_CHATGPT_AUTH_FILE`, then the platform configuration directory — only the file name differs.

The reason is rotation. A ChatGPT refresh token rotates on use, so whichever process refreshes it
invalidates every other copy of that credential, including one exported into a secret store. Two
surfaces sharing one file break each other silently. If discovery lands on the shared file anyway —
which today only an exported `DEKOPON_CHATGPT_AUTH_FILE` can cause — the console refuses to start and
names the path. An explicit `--auth-file` accepts it deliberately, the same shape
`auth chatgpt export` uses for `--expose-credential`.

Create one with a separate device authorization:

```console
dekopon auth chatgpt login --auth-file ~/.config/dekopon/chatgpt-auth.console.json
dekopon console --subject dev.console.xavier
```

`--endpoint <URL>` uses any OpenAI-compatible chat-completions endpoint instead, with
`--api-key-env <NAME>` naming the environment variable holding its bearer token. As everywhere else
in Dekopon, that is a variable name and never a value.

### Diagnostics while it is drawing

`-v` and `-vv` still work, but the console owns the terminal in both directions: the alternate
screen *is* the terminal, so a diagnostic written to standard error lands inside a frame and stays
there. When the console is about to open and standard error is that same terminal, diagnostics are
discarded rather than drawn over the view. Redirect them to keep them:

```console
dekopon console --subject dev.console.xavier -vv 2> console.log
```

### What it shows about secrets

Provider credentials are never in this data. The broker resolves a symbolic `credential:` name from
its owner-only credentials file and injects the value inside its native HTTP engine, after guest
header validation; the model, the script, the component, and the invocation input all see nothing.
What the console redacts is what a model wrote or a provider returned — a header assembled by hand,
a token pasted into a turn, a row in a result. A match hides only the run that matched, so the text
around it survives, and revealing is one keystroke against one field rather than a mode, because a
revealed secret is in terminal scrollback afterwards.

All rendered text is stripped of terminal control sequences first. A pull-request title and an issue
body are attacker-controlled text arriving through a read-only capability.

## Configuration discovery

Paths are considered in this exact order:

1. `--config <PATH>`
2. `DEKOPON_CONFIG`
3. `$XDG_CONFIG_HOME/dekopon/config.yaml`
4. `$HOME/.config/dekopon/config.yaml`
5. `./dekopon.yaml`

An explicit or environment path is authoritative: if it cannot be read, the command fails rather than falling back. For default locations, the first existing regular file wins; a candidate that exists but cannot be examined is an error naming that path, not a silent fall-through to the next location. An empty environment variable is ignored. If no default exists, the error lists all searched paths.

The loader accepts JSON, a single YAML resource, a YAML sequence, or multiple YAML documents. It parses the file once and rejects unknown fields, malformed IDs, unsupported API versions, duplicate names, missing capability references, missing provider references, and an `agent.spec.providers` list that disagrees with the providers the agent's own capabilities route to.

Semantic problems are accumulated: the whole catalog is scanned and every problem is reported in one list, so a file with several mistakes takes one `dekopon validate` run to diagnose. Only a failure that makes continuing impossible — an unreadable file or invalid YAML — stops at the first error.

## Output behavior

List output is sorted by validated identifier. `name` output uses qualified names such as `agent/reviewer`. A singular JSON or YAML command emits one resource; a list emits a versioned `AgentList`, `CapabilityList`, or `ProviderList`. `config view` emits a canonical grouped catalog rather than preserving comments or original document order.

## Exit codes

| Code | Meaning |
|---:|---|
| `0` | Success |
| `1` | Configuration, validation, rendering, or general runtime failure |
| `2` | Command-line usage error (emitted by Clap, or by a bare invocation off a terminal) |
| `3` | Requested resource not found |

These codes are stable across `0.x` releases and are tied to the `dekopon.dev/v1alpha1` resource API rather than to a package version. `3` is distinguished from `1` deliberately so a script can tell "this resource does not exist" from "the catalog would not load"; a future API version is the only thing that would renumber them.

## Examples

```console
dekopon --config examples/local/dekopon.yaml get agents
dekopon --config examples/local/dekopon.yaml get agents -o wide
dekopon --config examples/local/dekopon.yaml get agent reviewer -o yaml
dekopon --config examples/local/dekopon.yaml get capabilities -o name
dekopon --config examples/local/dekopon.yaml describe agent reviewer
dekopon --config examples/local/dekopon.yaml validate
dekopon --config examples/local/dekopon.yaml config view --output json
dekopon auth chatgpt export --expose-credential --namespace dekopon | kubectl apply -f -
dekopon auth chatgpt export --expose-credential --format raw > chatgpt-auth.json
dekopon auth chatgpt login --auth-file ~/.config/dekopon/chatgpt-auth.console.json
dekopon --config examples/local/dekopon.yaml console --subject dev.console.xavier
```
