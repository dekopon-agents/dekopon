# Continual improvement

This document follows one agent from a session that went badly to a catalog change that fixes it. It explains the two mechanisms Dekopon gives an operator for that — skills and `suggest_improvement` — how they compose, and what is not built.

**Status: current.** Every command, event, and bound named here exists and is tested. The model reads and reports, the telemetry backend records, and a person decides what changes.

## Answers at a glance

| Question | Current answer |
|---|---|
| How does an agent get knowledge it did not have? | An operator writes a skill — a directory holding a `SKILL.md` — and mounts it in the catalog. The model sees the skill's name and one-line description in every prompt and reads the rest on demand through `read_skill`. |
| How does an operator learn what the agent lacked? | With `suggest_improvement` enabled, the model records at most three bounded, typed notes per session, written to telemetry as `agent.improvement.suggested`. They are advice: nothing moves because a model asked. |
| Does anything improve itself? | **No.** No prompt is rewritten, no skill is generated, no suggestion is applied, and nothing a session learned outlives it except as a telemetry record a person reads. |
| Where do the artifacts live? | Skills and instructions in the catalog; accounting, transcripts, and suggestions in the telemetry backend. |

## Two mechanisms

| Mechanism | Where it lives | What the model can do with it | What it never does |
|---|---|---|---|
| Skills | The catalog's `spec.skills` directories and the agent a `dekopond` route binds | Read operator-authored instructions and resource files on demand | Grant authority, hold a secret, change between sessions |
| `suggest_improvement` | The session, when the embedder opted in; the record in the telemetry backend | Record a typed, bounded note for the operator | Change an instruction, skill, limit, or grant; reach the person in chat |

### Skills: progressive disclosure of operator-authored knowledge

A skill is a directory named after the skill, holding a `SKILL.md` — YAML front matter with `name` and `description`, then Markdown instructions — and, optionally, supporting files beside it, each addressed by its `/`-separated relative path. It is the Agent Skills directory layout: a `SKILL.md` that uses the specification's front-matter keys loads here unchanged, and a key the specification does not define is refused rather than ignored. [`catalog.md`](catalog.md#skills-are-directories-the-model-reads-on-demand) is the field-by-field contract and carries every bound. [`examples/catalog/skills/pull-request-review`](../examples/catalog/skills/pull-request-review/SKILL.md) is one, mounted by the `reviewer` agent in [`examples/catalog/dekopon.yaml`](../examples/catalog/dekopon.yaml); its body tells the model to read the `references/risk-checklist.md` resource only when a diff touches authorization, credentials, or an external write.

Where a skill lives decides who mounts it. An agent's `spec.skills` names directories relative to the catalog file, and the loader reads every one whole at catalog load. `dekopond` binds the loaded skills to every route naming the agent and mounts them on every session of that route, shared rather than re-read, so a session never touches the filesystem.

The model meets a skill in three steps, each paid for only when the model decides it needs it:

1. **The listing.** When at least one skill is mounted, a second system message follows the standing instructions. It begins `Skills mounted for this agent`, lists each skill as `- name: description`, and tells the model to call `read_skill` before doing work a skill covers. Nothing else of the skill is in the prompt.
2. **The instructions.** `read_skill` with `name` returns the skill's body, framed with its name and description and followed by the list of its resource files.
3. **A resource.** `read_skill` with `name` and `resource` returns one supporting file's text.

The listing is the trigger. The description is the one line the model matches a request against, which is why the format asks authors to write it as a "use when" sentence and why it is the only part that sits in every prompt. The body waits behind the tool because a tool result stays in the message vector and is re-sent on every later turn: a skill the model does not need for this request would otherwise cost its full length on every turn of every session. For the same reason a second read of the same instructions or resource within one session is answered with a one-line pointer at the earlier result rather than a second copy. A `SKILL.md` past 64 KiB is refused at load; a long checklist belongs in a resource the body names, read only when the case arises, which is what the example skill does with its risk checklist.

The listing is deterministic for one mounted set, and it sits with the instructions rather than with the request because it is agent-standing rather than request-scoped. On a `dekopond` route that keeps the leading prompt identical across sessions — the same instructions, the same listing, then what the conversation remembers and what the sender just said — so mounting a skill does not disturb the prompt-cache affinity [`inference.md`](inference.md#prompt-cache-key-lifecycle) describes. A `read_skill` result lands after that prefix, in the turn that read it.

An unknown skill name, or an unknown resource path, is a refusal the model reads and can recover from — it names the mounted skills, or the skill's resource files — and the session continues; `agent.skill.refused` records the reason (`unknown-skill` or `unknown-resource`) and never the name the model typed. Malformed arguments (not a JSON object, no `name`, an unexpected or mistyped field) end the session as a malformed call to any other tool does. Each successful read fires `agent.skill.read` with the operator-authored `skill.name`, the `skill.resource` path (empty for the body), the byte count, and whether it repeated an earlier read. Names and paths are operator-authored; the skill text reaches telemetry only inside the transcript events, which are always recorded ([goal 2](design.md#constitution)). `inspect_agent_config` shows the mounted skills as `skills: [{name, description, resources}]` — names and paths, never the text.

A skill shapes an answer and grants nothing. Authority is only what the broker attests for this sender and this agent, so a skill that tells the model to reach for a capability it was not granted yields `command not found` like any other ungranted capability. The model can read every mounted skill in full, so nothing secret goes in one — no token, credential, or internal hostname.

### Tap the glass: `suggest_improvement`

An agent that hit a limit, reached for a capability it was never granted, or found its standing instructions wrong has learned something its operator would pay to know — and can otherwise say so only in chat, to a person who may not be the operator. `suggest_improvement` gives that observation a typed shape and a tagged telemetry record, so an operator can aggregate a month of sessions by category and target instead of reading transcripts.

**It is opt-in everywhere, and the opt-in is consent.** The tool is never offered unless the embedder asked for it: `improvementSuggestions: true` on a `dekopond` route ([`dekopond.md`](dekopond.md#configuration)). The record carries model-authored text — a suggestion nobody can read is not a suggestion — so offering the tool is what declares the telemetry sink in scope for that text. Nothing else widens with it: the record carries no chat text the gateway holds and no subject, only what the model chose to write into six bounded fields.

The tool's own description tells the model when to call it: after the task is done or when it is genuinely blocked, at most three times per session, never instead of answering, and that the note goes to the operator's telemetry rather than to the person it is talking with. A call is a JSON object of six strings:

| Field | Bound | Meaning |
|---|---|---|
| `category` | `instructions`, `skill`, `capability`, `tool`, `limits`, or `other` | What kind of operator-owned thing the note is about |
| `target` | 128 bytes | The specific thing: a skill name, a capability identifier, `instructions`, a limit name |
| `summary` | 512 bytes | One sentence: what was wrong or could be better |
| `evidence` | 2048 bytes | What the session observed that supports it — an exit code, a refusal, a missing fact |
| `proposal` | 2048 bytes | The concrete change: the instruction to add, the skill to write, the capability to grant, the limit to raise |
| `confidence` | `low`, `medium`, or `high` | A hunch; likely, from one session's evidence; the session demonstrated it |

Every text field is trimmed and stripped of control characters other than newline and tab before it is recorded, so a suggestion cannot forge log structure. A well-formed call is answered `Recorded suggestion N of 3 for the operator.` and fires `agent.improvement.suggested` with `model.turn`, `tool_call.index`, `suggestion.index`, the enum tokens `suggestion.category` and `suggestion.confidence`, and the four text fields. A well-formed call that fails a bound — a token outside its enum, an empty field, a field past its bytes — is answered `Suggestion not recorded: …` naming the bound, and a fourth in one session is told the session has already recorded its three; each fires `agent.improvement.refused` with a fixed `reason` (`invalid-category`, `invalid-confidence`, `empty-field`, `field-too-long`, or `session-limit`) and none of the text, and the session continues: a suggestion is advisory, and the task it was about must not fail because the note was formatted badly. Only malformed arguments — not JSON, not an object, not the six-field shape — end the session, as they do for every tool.

Where a suggestion goes depends on who ran the session; in no case is it applied:

- `dekopond` relays nothing to chat; the sender sees only the answer. The record exists in telemetry alone.
- An embedder of `dekopon-agent` receives them as `PromptOutcome.suggestions`, already written to telemetry by the time they arrive.

Reading them back is one query against the stream the exporters wrote to. OpenObserve stores the `audit.event` attribute as `audit_event`, folding every character outside letters, digits, and underscores:

```sql
SELECT * FROM "dekopon" WHERE audit_event = 'agent.improvement.suggested'
```

Group by `suggestion_category` and `suggestion_target` to see what a fleet keeps asking for; `trace_id` joins a suggestion to the session that made it. What comes back is advice from an untrusted model about its own configuration. A `capability` suggestion is a request for authority and gets the policy review any other would; an `instructions` or `skill` suggestion is a draft an operator turns into a catalog edit. Nothing reads these records but a person.

## What is absent by decision

Each of these is a decision, not a gap. Every artifact of the loop is either in the catalog, reviewed like the rest of it, or in the telemetry backend, retained and protected like the rest of it; nothing sits in a third place with a lifecycle of its own.

- **No durable suggestion store.** A suggestion lives in the telemetry record that carries it and, for the process that ran the session, in `PromptOutcome.suggestions` until that process exits. Nothing keeps a queue of pending suggestions, marks one applied, or reads yesterday's back into today's session. The telemetry backend already is the store, with the retention, access control, and full-text search an operator configured for the rest of the sink; a second store would be one more place model-authored text lives and one more thing to redact, back up, and expire.
- **No automatic prompt rewriting.** Nothing reads `agent.improvement.suggested` and edits `instructions` or writes a `SKILL.md`. Standing instructions and skills are operator-authored text that every later session on the route obeys, and letting one session's output edit them would make model text a channel into standing configuration with no review — the shape of thing the [constitution](design.md#constitution) exists to refuse.
- **No grader.** A person reviews suggestions and owns every catalog change.
- **No cross-session memory.** A session's skill reads and suggestions are its own: `read_skill` returns a text once per session, the three-suggestion bound is per session, and nothing learned in one session reaches the next except through the operator's edit. The gateway's bounded conversation window and the optional durable chat-turn provider in [`inference.md`](inference.md#three-different-mechanisms) exist for a person's follow-up question, not for the agent improving itself, and neither carries a suggestion or a skill read.

## Related documents

- [`dekopond.md`](dekopond.md#sessions) — how a route mounts its agent's catalog skills and opts into `improvementSuggestions`, and why nothing a suggestion records reaches chat.
- [`catalog.md`](catalog.md#skills-are-directories-the-model-reads-on-demand) — the `spec.skills` field, the `SKILL.md` front matter, every bound, and what the loader refuses.
- [`observability.md`](observability.md#refusals-errors-and-outcomes) — `agent.skill.read`, `agent.skill.refused`, `agent.improvement.suggested`, and `agent.improvement.refused`; transcript payloads remain opt-in.
- [`inference.md`](inference.md) — the prompt-cache prefix a stable skills listing preserves, and the conversation memory that is not an improvement mechanism.
- [`security-model.md`](security-model.md) — why operator-authored text handed to a model shapes answers and grants nothing.
