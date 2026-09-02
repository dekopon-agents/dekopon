# CLI reference

`dekopon` is the operator interface for local catalog inspection and model-account lifecycle. Catalog commands are synchronous, read a validated local catalog, and contact no other process; `auth` commands instead contact the selected model provider's fixed authentication endpoint. No command here reaches `dekopon-brokerd` or `dekopond`. The interactive terminal console that used to live here is now [dekopon-console](https://github.com/dekopon-agents/dekopon-console), an out-of-tree unprivileged broker client with its own release cadence. The separate experimental `dekopon-run` executable loads read-only Wasm providers and is documented in [`run.md`](run.md). `dekopon-brokerd provider` is a distinct offline deployment-lifecycle surface rather than part of this catalog CLI; its exact-reference sync/list/verify contract is in the [broker operations manual](../crates/dekopon-brokerd/README.md#managed-provider-sets), alongside the equally offline `dekopon-brokerd audit verify --audit-path <PATH>`, which checks a durable audit chain without starting the broker.

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
```

`dekopon` with no subcommand prints `error: a subcommand is required` to standard error and exits `2`. The subcommand is optional
in the grammar only so that refusal comes from this CLI, naming what was missing, rather than from
Clap's generic required-argument error.

Run `dekopon --help` or `dekopon <COMMAND> --help` for generated syntax.

## Global flags

- `--config <PATH>`: authoritative YAML or JSON source for catalog commands; ignored by `version` and `auth`.
- `-o, --output <FORMAT>`: `table` (default), `wide`, `json`, `yaml`, or `name`.
- `--no-color`: disable ANSI color in diagnostics.
- `--quiet`: suppress successful output; errors still print. Conflicts with `-v`.
- `-v`: emit informational diagnostics and error causes.
- `-vv`: emit debug diagnostics and debug error context.

Global flags may appear before or after subcommands. Authentication commands do not load the catalog. `--output json` or `--output yaml` keeps authentication status machine-readable; device-login instructions are written to standard error so standard output remains parseable. `--output` does not apply to `auth chatgpt export`, whose form is chosen by `--format`.

## ChatGPT subscription authentication

`dekopon auth chatgpt login` uses OpenAI's Codex device authorization flow and writes only to Dekopon's credential file. `status` reports state without revealing tokens, and `logout` removes only Dekopon's file. The credential file is resolved in this exact order: `--auth-file <PATH>`, `DEKOPON_CHATGPT_AUTH_FILE`, `$XDG_CONFIG_HOME/dekopon/chatgpt-auth.json`, `$HOME/.config/dekopon/chatgpt-auth.json`, then `%APPDATA%/dekopon/chatgpt-auth.json`; when no tier applies the command fails asking for `DEKOPON_CHATGPT_AUTH_FILE`. Discovery treats a variable exported with an empty value as unset and falls through to the next tier, and refuses a discovered path that is not absolute — a relative `DEKOPON_CHATGPT_AUTH_FILE` or `XDG_CONFIG_HOME` would otherwise leave the rotating refresh token in whatever directory the process started in. Only `--auth-file` is taken verbatim. See [`run.md`](run.md) for inference behavior and the complete security boundary.

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
| `--namespace <NAMESPACE>` | Secret namespace; validated as an RFC 1123 label (no dots, at most 63 characters) before the credential is read; omitted from the manifest when unset. |
| `--expose-credential` | Required acknowledgement that this prints a live access token and refresh token. |
| `--allow-terminal` | Print to a terminal anyway. |

`--quiet` is refused, because suppressing the document while exiting `0` is how a scripted seeding step stores nothing and believes it succeeded.

The manifest carries the document under the key `chatgpt-auth.json`, matching Dekopon's own file name. Missing, malformed, incomplete, and unsupported-version credential files all fail with exit code `1` and print nothing, so a seeding step never stores a half-formed secret.

The refresh token rotates, so an exported copy is invalidated by the next refresh of the credential it came from. [`chatgpt-credential.md`](chatgpt-credential.md) is the full deployment lifecycle: export, store, seed once into a writable directory, and re-export only on a deliberate rotation.

## Configuration discovery

Paths are considered in this exact order:

1. `--config <PATH>`
2. `DEKOPON_CONFIG`
3. `$XDG_CONFIG_HOME/dekopon/config.yaml`
4. `$HOME/.config/dekopon/config.yaml`
5. `./dekopon.yaml`

An explicit or environment path is authoritative: if it cannot be read, the command fails rather than falling back. For default locations, the first existing regular file wins; a candidate that exists but cannot be examined is an error naming that path, not a silent fall-through to the next location. An empty environment variable is ignored. If no default exists, the error lists all searched paths.

The loader accepts JSON, a single YAML resource, a YAML sequence, or multiple YAML documents. It parses the file once and rejects unknown fields, malformed IDs, unsupported API versions, duplicate names, missing capability references, missing provider references, an `agent.spec.providers` list that disagrees with the providers the agent's own capabilities route to, and any `agent.spec.skills` directory that does not load; [`catalog.md`](catalog.md#what-the-loader-checks) is the complete list.

Semantic problems are accumulated: the whole catalog is scanned and every problem is reported in one list, so a file with several mistakes takes one `dekopon validate` run to diagnose. Only a failure that makes continuing impossible — an unreadable file or invalid YAML — stops at the first error. A document that never loads — a missing or unsupported `kind`, an unsupported `apiVersion`, a decode failure, or a malformed name — also suppresses the cross-reference checks for that run, because a resource that never loaded cannot be referenced by name; skill directories are still checked.

## Output behavior

List output is sorted by validated identifier. `name` output uses qualified names such as `agent/reviewer`. A singular JSON or YAML command emits one resource; a list emits a versioned `AgentList`, `CapabilityList`, or `ProviderList`. `config view` emits a canonical grouped catalog rather than preserving comments or original document order. `validate` prints `configuration valid: N agent(s), N capability(ies), N provider(s)` (`wide` adds a `source:` line, `name` prints `config/valid`) or a `{source, valid, agents, capabilities, providers}` document in `json`/`yaml`; an invalid catalog is an error with exit `1`, never a `valid: false` document. `version` prints `dekopon <version>` or `{product, version, apiVersion}`.

`describe agent` expands one agent with the capabilities and providers it names and the skills it mounts. The text form — `table` and `wide` render it identically — ends with a `Skills:` heading after `Providers:`, listing each skill as `  - <name> [<N> resource file(s)]: <description>` in the order `spec.skills` names them, or `  (none)`. Name and description are what a model matches a request against and the count says how much sits behind `read_skill`; the skill's body and its resource text are never printed there. The JSON and YAML forms carry each loaded skill whole under `skills` — `name`, `description`, `body`, `resources` as `path` and `text` pairs, the front matter's optional `license`, `compatibility`, `metadata`, and `allowedTools`, and the resolved `source` directory — and omit the key when the agent mounts none, so the exact text a session will read can be diffed. The wide agent table (`get agents -o wide`, `get agent <NAME> -o wide`, the `AGENTS` group of `config view -o wide`) inserts `PROVIDERS`, `SKILLS`, `MODEL`, and `POLICY` between `CAPABILITIES` and `DESCRIPTION`; `SKILLS` counts the directories `spec.skills` names, which `get agent -o yaml` prints as written in the catalog and omits when empty.

## Exit codes

| Code | Meaning |
|---:|---|
| `0` | Success |
| `1` | Configuration, validation, rendering, or general runtime failure |
| `2` | Command-line usage error (emitted by Clap, or by a bare invocation naming no subcommand) |
| `3` | Requested resource not found |

These codes are stable across `0.x` releases and are tied to the `dekopon.dev/v1alpha1` resource API rather than to a package version. `3` is distinguished from `1` deliberately so a script can tell "this resource does not exist" from "the catalog would not load"; a future API version is the only thing that would renumber them.

## Examples

```console
dekopon --config examples/local/dekopon.yaml get agents
dekopon --config examples/local/dekopon.yaml get agents -o wide
dekopon --config examples/local/dekopon.yaml get agent reviewer -o yaml
dekopon --config examples/local/dekopon.yaml get capabilities -o name
dekopon --config examples/local/dekopon.yaml describe agent reviewer
dekopon --config examples/local/dekopon.yaml describe agent reviewer -o json
dekopon --config examples/local/dekopon.yaml validate
dekopon --config examples/local/dekopon.yaml config view --output json
dekopon auth chatgpt export --expose-credential --namespace dekopon | kubectl apply -f -
dekopon auth chatgpt export --expose-credential --format raw > chatgpt-auth.json
```
