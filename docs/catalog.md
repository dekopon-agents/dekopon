# Catalog resource reference

**Status: current.** This is the field-by-field contract for the `dekopon.dev/v1alpha1` catalog —
the `Agent` documents in the file `dekopond`'s `catalogPath` points at. [`cli.md`](cli.md) covers
the separate model-auth commands; this document covers the schema and, for every field, what
actually consumes it today.

That last part is the reason this document exists. The catalog looks like a permission system and is
not one. Four of an agent's fields decide how it behaves and the rest are authored names that no
shipped component reads. Authoring one correctly means knowing which is which.

## Who reads it

| Process | Reads the catalog? | What it does with it |
|---|---|---|
| `dekopond` | Yes, at startup | Binds each route to an agent, resolves that agent's model, hands its `instructions` to the model as a system prompt, and mounts its `skills` on every session the route serves. |
| `dekopon-brokerd` | **No** | The broker does not link `dekopon-config` and never sees this file. It declares the `Dekopon::Agent` Cedar type and matches instances by name without enumerating them. |

The consequence worth internalizing: **nothing an agent may actually do comes from this file.** The
broker's `constraintSets` and Cedar policy decide that, and neither reads the catalog. The
capabilities a session may reach come from the broker, which builds them from the provider
manifests it loaded; an agent's `capabilities` list here is a declaration of intent that grants
nothing and is compared against nothing, and a name misspelled in a
policy's `Dekopon::Agent::"…"` literal cannot be caught by validating this file — see
[`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#catalog-ownership-at-policy-startup).

## The document envelope

An agent is one YAML or JSON document with four keys. The loader accepts JSON, a single YAML
document, a YAML sequence, or a multi-document YAML stream, and it parses the file once.

```yaml
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
  labels:              # optional
    environment: local
spec: { … }            # see below
status: Ready          # optional
```

| Field | Required | Notes |
|---|---|---|
| `apiVersion` | yes | Exactly `dekopon.dev/v1alpha1`. Any other value fails to decode. |
| `kind` | yes | Exactly `Agent`. Any other value is a load failure naming the document. |
| `metadata.name` | yes | Validated as an agent identifier. |
| `metadata.labels` | no | A string-to-string map with stable ordering. Stored by protocol `ObjectMeta` serde; no shipped selector, filter, or inventory reader consumes it (source map below). |
| `spec` | yes | The agent's desired state, below. |
| `status` | no | Authored, never observed — see [Reserved and inert fields](#reserved-and-inert-fields). |

Authored structures **reject unknown fields**. A misspelled key is a load failure naming the
document, not a silently ignored setting: the catalog is security-adjacent configuration, and
quietly dropping `capabilties:` would be worse than refusing the file.

### Identifier grammar

`metadata.name`, and every capability and provider identifier an agent names, is validated by the
same rule:

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
| `description` | string | yes | Bound into the gateway route and returned by `inspect_agent_config`. |
| `enabled` | bool | no, defaults `true` | **Load-bearing in `dekopond`.** A route naming a disabled agent is a startup failure. Authored status does not override it. |
| `instructions` | string | no | **Load-bearing in `dekopond`.** Handed to the model verbatim as the session's system prompt. Absent means the agent runs with no standing orders. |
| `skills` | list of directory paths | no | **Load-bearing in `dekopond`.** Each names a skill directory — relative paths resolve against the catalog file's own directory — that the loader reads whole at load time. `dekopond` mounts them on every session of a route bound to the agent. See [`skills` are directories the model reads on demand](#skills-are-directories-the-model-reads-on-demand). |
| `capabilities` | list of capability IDs | no | **Reserved.** Stored typed catalog metadata read by nothing; the capabilities a session reaches come from the broker. See [Reserved and inert fields](#reserved-and-inert-fields). |
| `providers` | list of provider IDs | no | **Reserved.** Stored typed catalog metadata read by nothing. See [Reserved and inert fields](#reserved-and-inert-fields). |
| `modelClass` | string | no, but see below | **Load-bearing in `dekopond`.** Selects which configured model serves the agent. |
| `policyProfile` | string | no | **Reserved.** Nothing reads it. See [Reserved and inert fields](#reserved-and-inert-fields). |
| `status` | `Ready` \| `Pending` \| `Disabled` \| `Error` | no | **Reserved.** Stored typed authored metadata, never observed or reported; omission stays `None`, with no presentation fallback. |

### `instructions` is untrusted model text, and it is readable

Standing orders shape how an agent answers and nothing else. They cannot assert identity, name a
principal, widen a capability, or influence an authorization decision — broker policy never reads
this field. Treat the text the way you would treat any other model input.

They are also not private. An authorized chat sender retrieves them verbatim through the
gateway's `inspect_agent_config` tool, which exists to expose them. **Do not put a secret, a
token, or an internal hostname in `instructions`.**

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
| `license` | no | Stored by `Skill::license`; no shipped presentation reader. Blank is treated as absent. |
| `compatibility` | no | Declared environment requirements stored by `Skill::compatibility`; no shipped presentation reader; blank is treated as absent. |
| `metadata` | no | A map of scalar values — strings, booleans, numbers, or null — kept as text. A list or map value is refused. |
| `allowed-tools` | no | A string stored by `Skill::allowed_tools`; no shipped presentation reader. **Not enforced:** a session has one scripting tool whatever a skill says, and authority comes from broker policy, never from a file a model reads. |

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
  text.

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

It is optional for an unrouted agent or a route with an explicit model. Gateway
`RoutingTable::bind` requires it only when the route does not name a model:

- a route that names `model:` explicitly overrides the class, and then `modelClass` selects nothing
  (`inspect_agent_config` returns it regardless);
- a route with no `model:` and an agent with no `modelClass` is a **`dekopond` startup failure**;
- a route with no `model:`, an agent with a `modelClass`, and no configured model offering that
  class is also a startup failure.

Failing at startup rather than per-session is the point: a catalog typo here is one refused boot, not
an agent that appears configured and answers nobody. See
[`dekopond.md`](dekopond.md#configuration) for the model list and route syntax.

## Reserved and inert fields

Five fields are decoded and retained as typed metadata with no shipped behavioral reader. Each one
reads like it selects a behavior, so each is listed here rather than left to be discovered.

| Field | Looks like | Actually |
|---|---|---|
| `spec.capabilities` | The operations the agent may propose | Authored intent, compared against nothing. What a session may reach is the broker's answer to `capabilities` under that agent's attestation, built from the loaded provider manifests and the `constraintSets` policy allows. Adding a name here reaches nothing new; removing one narrows nothing. |
| `spec.providers` | The integrations the agent uses | Authored intent, compared against nothing. A capability's provider is fixed by the manifest that declares it, and the broker selects it. |
| `spec.policyProfile` | Selects a named policy for the agent | Not consumed by runtime authority. Broker authority comes from the owner-authored Cedar policy file and the per-capability `constraintSets` in `broker.yaml`; naming a profile here selects no policy and changes no decision. |
| `status` | Observed availability | Authored. No probe, daemon, or reconciler ever writes it, so the catalog records the file, not the deployment. |
| `metadata.labels` | Selection or grouping | Retained by protocol serde. Nothing filters, selects, or reports on them. |

All five are optional and may simply be omitted. They are worth authoring only as documentation a
reviewer reads to understand what the deployment intends, and a reviewer should know that the
broker's configuration can disagree with every one of them without either process noticing.

### Source map for stored metadata and shipped readers

- [`dekopon-protocol/src/lib.rs`](../crates/dekopon-protocol/src/lib.rs):
  `ObjectMeta`, `AgentSpec` and `AgentStatus` own the typed serde storage of labels,
  policyProfile, capability and provider names, descriptions and the optional authored status.
  Storage and serialization are not a catalog display command.
- [`dekopond/src/routes.rs`](../crates/dekopond/src/routes.rs), `RoutingTable::bind`:
  checks enabled, resolves explicit model or modelClass, and binds instructions and loaded skills.
- [`dekopon-config/src/skill.rs`](../crates/dekopon-config/src/skill.rs), `Skill` and `load_skill`:
  retain license, compatibility, scalar metadata and allowed-tools with typed accessors.
  [`dekopon-agent/src/skills.rs`](../crates/dekopon-agent/src/skills.rs), `prompt_block` and
  `render_skill`, use name, description, body and resource paths/text, not those optional
  front-matter fields. [`dekopond/src/session.rs`](../crates/dekopond/src/session.rs)
  constructs self-inspection with name, description and resource paths only. The gateway mounts
  the loaded skills through the shared agent layer; no surviving renderer promises to
  display the optional front matter. Metadata scalars remain converted to text by the loader.

## What the loader checks

Loading is a single pass that either produces a fully validated catalog or fails with one error
naming the file and each offending document or skill directory:

- the file is non-empty and parses as JSON or YAML;
- every document carries a `kind`;
- every document's `kind` is `Agent`, and it decodes with no unknown fields and an accepted
  `apiVersion`;
- `metadata.name` is a valid agent identifier, and every capability and provider name the agent
  lists is a valid identifier of its own type;
- no two documents share a name;
- every `agent.spec.skills` entry, resolved against the catalog file's directory when relative,
  loads as a skill directory as described above. Every skill that does not is reported — naming
  the agent, the authored path, and the file at fault — in the same refusal, so an operator with
  three broken skills fixes three and validates once;
- no two skills one agent mounts share a `name`; the second is refused rather than shadowing the
  first, because a model could not tell two `read_skill` targets apart.

`dekopon-config` runs these checks whenever a catalog loads. A catalog is either wholly
loadable or wholly refused — there is no partial mode where some resources are usable.

What it does not check is whether any of it is true. The broker owns the capability surface, and
a catalog that disagrees with it produces no error here and no error there.

## Related documents

- [`cli.md`](cli.md) — model-auth formats and exit codes.
- [`dekopond.md`](dekopond.md) — routes, model endpoints, sessions, and conversations; the consumer
  that makes `instructions`, `skills`, `enabled`, and `modelClass` load-bearing.
- [`improvement.md`](improvement.md) — catalog-mounted skills and opt-in suggestions.
- [`dekopon-brokerd` § Boundaries](../crates/dekopon-brokerd/README.md#boundaries) —
  `constraintSets`, Cedar policy, and the separate broker configuration that decides authority.
- [`examples/catalog/dekopon.yaml`](../examples/catalog/dekopon.yaml) — a complete authored catalog.
- [`examples/catalog/skills/pull-request-review/SKILL.md`](../examples/catalog/skills/pull-request-review/SKILL.md)
  — the skill that catalog's `reviewer` agent mounts, with one resource file.
