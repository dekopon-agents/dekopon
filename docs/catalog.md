# Catalog resource reference

**Status: current.** This is the field-by-field contract for the `dekopon.dev/v1alpha1` catalog —
the `Agent`, `Capability`, and `Provider` documents in the file `dekopon --config` and `dekopond`'s
`catalogPath` both point at. [`cli.md`](cli.md) covers discovery, output formats, and exit codes;
this document covers the schema and, for every field, what actually consumes it today.

That last part is the reason this document exists. The catalog looks like a permission system and is
not one. Three of its fields decide how an agent behaves, several are validated cross-references,
and four are reserved names that no shipped component reads. Authoring one correctly means knowing
which is which.

## Who reads it

| Process | Reads the catalog? | What it does with it |
|---|---|---|
| `dekopon` | Yes | Renders and validates it, and with it every skill directory an agent's `spec.skills` names, because loading the catalog reads them. Every catalog command is this file and those directories, nothing else. |
| `dekopond` | Yes, at startup | Binds each route to an agent, resolves that agent's model, hands its `instructions` to the model as a system prompt, mounts its `skills` on every session the route serves, and publishes a bounded content-free inventory to the broker's web UI. |
| `dekopon-brokerd` | **No** | The broker does not link `dekopon-config` and never sees this file. It declares the `Dekopon::Agent` Cedar type and matches instances by name without enumerating them. |
| `dekopon-run` | **No** | The runner loads Wasm components by path and has no catalog concept; it links `dekopon-config` only for `load_skill`, which its `--skill <DIRECTORY>` flag uses to mount a skill directory in the format below without a catalog. |

The consequence worth internalizing: **nothing an agent may actually do comes from this file.** The
broker's `constraintSets` and Cedar policy decide that, and neither reads the catalog. An agent's
`capabilities` list is a declaration of intent that grants nothing, and a name misspelled in a
policy's `Dekopon::Agent::"…"` literal cannot be caught by validating this file — see
[`broker-http.md`](broker-http.md#startup-validation).

## The document envelope

Every resource is one YAML or JSON document with the same four keys. The loader accepts JSON, a
single YAML document, a YAML sequence, or a multi-document YAML stream, and it parses the file once.

```yaml
apiVersion: dekopon.dev/v1alpha1
kind: Agent            # Agent | Capability | Provider
metadata:
  name: reviewer
  labels:              # optional
    environment: local
spec: { … }            # kind-specific; see below
status: Ready          # optional
```

| Field | Required | Notes |
|---|---|---|
| `apiVersion` | yes | Exactly `dekopon.dev/v1alpha1`. Any other value fails to decode. |
| `kind` | yes | Must match the document's own shape; a `spec` for one kind under another kind's name is a load failure. |
| `metadata.name` | yes | Validated as the kind's identifier type. |
| `metadata.labels` | no | A string-to-string map with stable ordering. **Consumed by nothing.** It round-trips through `-o yaml`/`-o json` and is never selected on, filtered by, or reported. |
| `spec` | yes | Kind-specific, below. |
| `status` | no | Kind-specific. Authored, never observed — see [Reserved and inert fields](#reserved-and-inert-fields). |

Authored structures **reject unknown fields**. A misspelled key is a load failure naming the
document, not a silently ignored setting. That is deliberate: the catalog is security-adjacent
configuration, and quietly dropping `capabilties:` would be worse than refusing the file.

### Identifier grammar

`metadata.name`, and every reference to one, is validated by the same rule for all three kinds:

- at most 253 bytes;
- lowercase ASCII letters and digits only, plus the separators `.`, `-`, and `_`;
- must start and end with a letter or digit;
- no two adjacent separators.

`reviewer`, `gh.pull-request.read`, and `memory-chat` are valid; `Reviewer`, `gh..read`, and
`-reviewer` are not. The error names the offending character and its byte offset.

## `Agent`

```yaml
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: Reviews pull requests and comments once
  enabled: true
  modelClass: reasoning
  instructions: |
    You review pull requests. Comment once, do not approve.
  skills:
    - skills/pull-request-review   # a directory beside this file, holding SKILL.md
  capabilities:
    - gh.pull-request.read
    - gh.pull-request.comment
  providers:
    - gh
status: Ready
```

| Field | Type | Required | What consumes it |
|---|---|---|---|
| `description` | string | yes | Rendered by `dekopon get`/`describe`, and reported in the broker web UI inventory (bounded to 4 KiB). |
| `enabled` | bool | no, defaults `true` | **Load-bearing in `dekopond`.** A route naming a disabled agent is a startup failure. It also overrides `status` in CLI rendering: a disabled agent always displays `Disabled`. |
| `instructions` | string | no | **Load-bearing in `dekopond`.** Handed to the model verbatim as the session's system prompt. Absent means the agent runs with no standing orders. |
| `skills` | list of directory paths | no | **Load-bearing in `dekopond`.** Each names a skill directory — relative paths resolve against the catalog file's own directory — that the loader reads whole at load time. `dekopond` mounts them on every session of a route bound to the agent; `dekopon describe agent` lists them by name, description, and resource count. See [`skills` are directories the model reads on demand](#skills-are-directories-the-model-reads-on-demand). |
| `capabilities` | list of capability IDs | no | Cross-checked at load: every entry must name a `Capability` in the same catalog or the file is rejected. Rendered by the CLI, expanded into the web UI inventory. **Grants nothing.** |
| `providers` | list of provider IDs | no | Cross-checked at load: every entry must name a `Provider`, and the list must match exactly the providers the agent's `capabilities` route to. Rendered and reported. **Grants nothing.** |
| `modelClass` | string | no, but see below | **Load-bearing in `dekopond`.** Selects which configured model serves the agent. |
| `policyProfile` | string | no | **Reserved.** Nothing reads it. See [Reserved and inert fields](#reserved-and-inert-fields). |
| `status` | `Ready` \| `Pending` \| `Disabled` \| `Error` | no | Authored, never observed. Rendered by the CLI; absent renders as `Pending`. |

### `instructions` is untrusted model text, and it is readable

Standing orders shape how an agent answers and nothing else. They cannot assert identity, name a
principal, widen a capability, or influence an authorization decision — broker policy never reads
this field. Treat the text the way you would treat any other model input.

They are also not private. An authorized chat sender can retrieve them verbatim through the
gateway's `inspect_agent_config` tool, which was added for exactly that purpose. **Do not put a
secret, a token, or an internal hostname in `instructions`.**

### `skills` are directories the model reads on demand

Each `spec.skills` entry names a directory in the Agent Skills layout: a directory named after the
skill, holding a `SKILL.md` and, optionally, supporting files beside it. A `SKILL.md` that uses the specification's front-matter keys loads here unchanged; a key the specification does not define is refused rather than ignored.

`SKILL.md` opens with YAML front matter between `---` lines — the opening fence is the first line
and the closing fence a line of its own; CRLF endings are tolerated — followed by the skill's
Markdown instructions, its *body*. The front matter is strict, like every other authored document
here: an unknown key is a load failure naming it.

| Key | Required | Notes |
|---|---|---|
| `name` | yes | Lowercase ASCII letters, digits, and single hyphens; at most 64 bytes; starts and ends with a letter or digit. **Must equal the directory's name.** The grammar is narrower than the catalog's [identifier grammar](#identifier-grammar) on purpose — it is the one the format fixes — so `pull-request-review` loads and `Pdf`, `pdf_tools`, and `pdf--tools` do not. |
| `description` | yes | Trimmed, non-blank, at most 1024 bytes. The one line a model reads to decide whether the skill applies; it sits in every prompt on a route that mounts the skill. |
| `license` | no | Recorded and rendered. Blank is treated as absent. |
| `compatibility` | no | Declared environment requirements. Recorded and rendered; blank is treated as absent. |
| `metadata` | no | A map of scalar values — strings, booleans, numbers, or null — kept as text. A list or map value is refused. |
| `allowed-tools` | no | A string, carried through and rendered. **Not enforced:** a session has one scripting tool whatever a skill says, and authority comes from broker policy, never from a file a model reads. |

Every other regular file in the directory tree is a *resource* of the skill, addressed by its
`/`-separated path relative to the skill directory, such as `references/risk-checklist.md`, and
sorted by that path. Hidden entries (a leading `.`) are skipped as editor and version-control
residue. A symbolic link anywhere in the tree, the skill directory itself included, is refused
rather than followed, because a link is how content escapes the directory that was reviewed; so is
anything that is neither a regular file nor a directory, and any file name or file content that is
not UTF-8.

Every bound is fixed before a byte is read: `SKILL.md` is at most 64 KiB including its front
matter; each resource is at most 256 KiB; a skill carries at most 64 resources, at most 1 MiB in
total, nested at most four directory levels below the skill directory. Everything within those
bounds is read into memory when the catalog loads, so a session never touches the filesystem to
show a model a skill, and a skill that cannot be read refuses the catalog rather than a session.

What consumes a loaded skill:

- `dekopond` binds an agent's skills to every route naming it and mounts them on every session on
  that route: a second system message after `instructions` lists each skill by name and
  description, and the `read_skill` tool returns a skill's body, or one resource's text, when the
  model asks. Bodies and resources are never in a prompt until read; each read is recorded as
  `agent.skill.read`. See [`dekopond.md`](dekopond.md#sessions) and
  [`observability.md`](observability.md).
- `inspect_agent_config` lists mounted skills by name, description, and resource paths — never the
  text. Skills are not part of the web UI inventory.
- `dekopon describe agent` renders a `Skills:` section, one `- <name> [<N> resource file(s)]:
  <description>` line per skill or `(none)`; `dekopon get agents -o wide` adds a `SKILLS` column
  counting the agent's `spec.skills` entries. `-o yaml`, `-o json`, and `config view` show the
  authored paths, never the skill text.
- `dekopon-run --skill <DIRECTORY>` mounts the same format with no catalog; see
  [`run.md`](run.md#mounting-skills).

A skill is operator-authored text handed to the model, exactly as `instructions` is. It shapes how
the agent answers and nothing else: it cannot widen a capability, name a principal, or influence an
authorization decision, and broker policy never reads it. It is also not private — the model reads
any mounted skill in full through `read_skill`, and an authorized sender can list skill names,
descriptions, and resource paths through `inspect_agent_config`. **A skill must hold no secret: no
token, credential, or internal hostname belongs in a `SKILL.md` or in any resource file.**

### `modelClass` decides which model runs the agent

`dekopond`'s configuration lists model endpoints, each declaring the classes it satisfies. For each
route, the agent's `modelClass` picks the first configured model offering that class, in declaration
order, so an operator controls preference by ordering `models` rather than by a hidden score.

It is optional in the schema only because an agent the gateway never routes — one read by the CLI
alone — does not need one. For a routed agent it is effectively required:

- a route that names `model:` explicitly overrides the class, and then `modelClass` selects nothing
  (it is still reported to the web UI and to `inspect_agent_config`);
- a route with no `model:` and an agent with no `modelClass` is a **`dekopond` startup failure**;
- a route with no `model:`, an agent with a `modelClass`, and no configured model offering that
  class is also a startup failure.

Failing at startup rather than per-session is the point: a catalog typo here is one refused boot, not
an agent that appears configured and answers nobody. See
[`dekopond.md`](dekopond.md#configuration) for the model list and route syntax.

## `Capability`

```yaml
apiVersion: dekopon.dev/v1alpha1
kind: Capability
metadata:
  name: gh.pull-request.comment
spec:
  description: Post a pull-request review comment as an explicit external write
  provider: gh
  effect: external-write
  risk: High
  idempotency: conditional
  permissions:
    - operation: pull_requests:write
status: Unknown
```

| Field | Type | Required | What consumes it |
|---|---|---|---|
| `description` | string | yes | Rendered by the CLI and reported in the web UI inventory. |
| `provider` | provider ID | yes | Cross-checked at load: must name a `Provider` in the same catalog. |
| `effect` | `read-only` \| `local-write` \| `external-write` | yes | Rendered by the CLI in the default and wide capability tables. |
| `risk` | `Low` \| `Medium` \| `High` \| `Critical` | yes | Rendered by `-o wide`. |
| `idempotency` | `idempotent` \| `conditional` \| `non-idempotent` | yes | Rendered by `-o wide`. |
| `permissions` | list of `{ operation, resource? }` | no | Rendered as a count by `-o wide`, expanded in the web UI inventory. |
| `status` | `Available` \| `Unavailable` \| `Unknown` | no | Authored, never observed; absent renders as `Unknown`. |

**The broker does not read any of this.** The trusted `effect`, `risk`, and `idempotency` a policy
decision actually sees come from the capability's `constraintSets` entry in `broker.yaml`, validated
against the loaded provider manifest — as does its `route`, the field that decides whether a
capability is ordinary or part of the reserved chat-memory surface. The catalog's copy is operator documentation: it is what a
reviewer reads to understand what the deployment intends, and it can disagree with the broker
without either process noticing. When they disagree, the broker's copy is the one that decides.

## `Provider`

```yaml
apiVersion: dekopon.dev/v1alpha1
kind: Provider
metadata:
  name: gh
spec:
  description: GitHub provider for repository and pull-request operations
  type: github
  credentialRef: github-pat
status: Unknown
```

| Field | Type | Required | What consumes it |
|---|---|---|---|
| `description` | string | yes | Rendered by the CLI. |
| `type` | string | yes | Free-form implementation family, such as `github`. Rendered by the CLI; matched against nothing. |
| `credentialRef` | string | yes | **Reserved.** Nothing resolves it. See below. |
| `status` | `Ready` \| `Unavailable` \| `Unknown` | no | Authored, never observed; absent renders as `Unknown`. |

A catalog `Provider` is not the Wasm component. The component is a `.wasm` file the broker loads by
path from its own `providers:` list, and its manifest — not this document — declares the capability
IDs, input schemas, and command words the broker trusts. This resource is the operator-facing
declaration that such a provider is part of the deployment.

## Reserved and inert fields

Four fields are authored, validated, rendered, and consumed by nothing. They are listed here rather
than left to be discovered, because each one reads like it selects a behavior.

| Field | Looks like | Actually |
|---|---|---|
| `spec.policyProfile` (Agent) | Selects a named policy for the agent | Read by `dekopon get`/`describe` and nothing else. Broker authority comes from the owner-authored Cedar policy file and the per-capability `constraintSets` in `broker.yaml`; naming a profile here selects no policy and changes no decision. |
| `spec.credentialRef` (Provider) | Names the credential the provider will present | Rendered in the wide provider table (`dekopon get -o wide`, `config view -o wide`), carried through every `-o json`/`-o yaml` form, and read by nothing else. Legacy credential binding is owned by `constraintSets` (`credential:` / `credentialByAgent:`) and the broker's `0600` credentials file. Model-selected public DRNs are owned by the separate typed proposal/private-map/`secret.use` path. Neither mechanism consults this catalog field. A `credentialRef` that matches nothing is not an error, and one that matches a real credential name still binds nothing. |
| `status` (all three kinds) | Observed availability | Authored. No probe, daemon, or reconciler ever writes it, so `dekopon get capabilities` reports the file, not the deployment. |
| `metadata.labels` | Selection or grouping | Round-tripped through `-o yaml`/`-o json`. Nothing filters, selects, or reports on them. |

`credentialRef` is required by the schema, so a `Provider` document must carry one even though the
value is inert. `policyProfile`, `status`, and `labels` are optional and may simply be omitted.

## What the loader checks

Loading is a single pass that either produces a fully validated catalog or fails with one error
naming the file and each offending document or skill directory:

- the file is non-empty and parses as JSON or YAML;
- every document carries a `kind`;
- every document decodes into its kind with no unknown fields and an accepted `apiVersion`;
- `metadata.name` is a valid identifier for that kind;
- no two documents of one kind share a name;
- every `agent.spec.capabilities` entry names a `Capability` in this catalog;
- every `agent.spec.providers` entry and every `capability.spec.provider` names a `Provider` in this
  catalog;
- `agent.spec.providers` agrees with the agent's own capabilities: a provider that one of the
  agent's capabilities routes to must be listed, and a listed provider that none of its capabilities
  route to is refused (the second check runs only once every capability resolved);
- every `agent.spec.skills` entry, resolved against the catalog file's directory when relative,
  loads as a skill directory as described above. Every skill that does not is reported — naming
  the agent, the authored path, and the file at fault — in the same refusal, so an operator with
  three broken skills fixes three and validates once;
- no two skills one agent mounts share a `name`; the second is refused rather than shadowing the
  first, because a model could not tell two `read_skill` targets apart.

`dekopon validate` runs exactly this and reports the result; `dekopon get`, `describe`, and `config
view` run it before rendering anything. A catalog is therefore either wholly loadable or wholly
refused — there is no partial mode where some resources are usable.

## Related documents

- [`cli.md`](cli.md) — configuration discovery order, output formats, and exit codes.
- [`dekopond.md`](dekopond.md) — routes, model endpoints, sessions, and conversations; the consumer
  that makes `instructions`, `skills`, `enabled`, and `modelClass` load-bearing.
- [`run.md`](run.md#mounting-skills) — the runner's `--skill` flag, which mounts the same skill
  format with no catalog in the loop.
- [`broker-http.md`](broker-http.md) — `constraintSets`, Cedar policy, and why the broker's own
  configuration is what decides authority.
- [`crates/dekopon-brokerd/README.md`](../crates/dekopon-brokerd/README.md) — the broker
  configuration this catalog is deliberately separate from.
- [`examples/local/dekopon.yaml`](../examples/local/dekopon.yaml) — a complete authored catalog.
- [`examples/local/skills/pull-request-review/SKILL.md`](../examples/local/skills/pull-request-review/SKILL.md)
  — the skill that catalog's `reviewer` agent mounts, with one resource file.
