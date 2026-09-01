# Continual improvement

This document follows one agent from a session that went badly to a catalog change that fixes it. It explains the three mechanisms Dekopon gives an operator for that — skills, `suggest_improvement`, and session replay — how they compose, and what is deliberately not built.

**Status: Current.** Every command, event, and bound named here exists and is tested. Dekopon drives improvement without a durable suggestion store, without rewriting a prompt on its own, without a grader, and without memory that crosses sessions: the model reads and reports, the telemetry backend records, and a person decides what changes.

## Answers at a glance

| Question | Current answer |
|---|---|
| How does an agent get knowledge it did not have? | An operator writes a skill — a directory holding a `SKILL.md` — and mounts it in the catalog. The model sees the skill's name and one-line description in every prompt and reads the rest on demand through `read_skill`. |
| How does an operator learn what the agent lacked? | With `suggest_improvement` enabled, the model records at most three bounded, typed notes per session, written to telemetry as `agent.improvement.suggested`. They are advice: nothing moves because a model asked. |
| How is a change checked before it ships? | `dekopon-run session replay` puts a recorded session's prompt to a model again under the changed instructions or a newly mounted skill, answers the scripts it writes from the recording, and reports where the two runs part. No capability runs by default. |
| Does anything improve itself? | **No.** No prompt is rewritten, no skill is generated, no suggestion is applied, and nothing a session learned outlives it except as a telemetry record a person reads. |
| Where do the artifacts live? | Skills and instructions in the catalog; accounting, transcripts, and suggestions in the telemetry backend; a `session show --json` file wherever the operator keeps it. There is no third place. |

## The loop

```text
sessions run          dekopon-run prompt, or a dekopond route
      |               accounting in either payload mode; transcript with payloads on;
      |               agent.improvement.suggested when the tool was offered
      v
session list          which sessions ran, how many turns, which failed
      |
      v
session show          what one session saw, wrote, and was answered with
      |
      v
edit                  instructions in the catalog, or a SKILL.md and its resources
      |
      v
session replay        the same prompt and history under the changed instructions
      |               or the new skill; scripts answered from the recording
      v
compare               recorded and replayed scripts index by index, both answers
      |
      v
commit                spec.instructions and spec.skills; dekopon validate
```

Every arrow is a command an operator runs or an edit an operator makes. Skills feed the top of the loop, suggestions point at what to edit, and replay is how the edit is checked. The three are independent: a deployment can mount skills and never enable suggestions, or replay the sessions of an agent that mounts nothing.

## Three mechanisms

| Mechanism | Where it lives | What the model can do with it | What it never does |
|---|---|---|---|
| Skills | The catalog's `spec.skills` directories, `dekopon-run --skill`, the agent a `dekopond` route binds | Read operator-authored instructions and resource files on demand | Grant authority, hold a secret, change between sessions |
| `suggest_improvement` | The session, when the embedder opted in; the record in the telemetry backend | Record a typed, bounded note for the operator | Change an instruction, skill, limit, or grant; reach the person in chat |
| Session replay | `dekopon-run session`, reading the OpenObserve stream back | Nothing — it is the operator's tool | Run a capability by default, invent tool output, or score an answer |

### Skills: progressive disclosure of operator-authored knowledge

A skill is a directory named after the skill holding a `SKILL.md` — YAML front matter with `name` (equal to the directory name; lowercase ASCII letters, digits, and single hyphens, at most 64 bytes) and `description` (non-blank, at most 1024 bytes), then Markdown instructions — and, optionally, supporting files beside it, each addressed by its `/`-separated relative path. This is the Agent Skills directory layout, so a `SKILL.md` that uses the specification's front-matter keys loads here unchanged, while a key the specification does not define is refused rather than ignored. [`catalog.md`](catalog.md#skills-are-directories-the-model-reads-on-demand) is the field-by-field contract and carries every bound: a 64 KiB `SKILL.md`, 256 KiB per resource, 64 resources, 1 MiB in all, four directory levels; hidden files skipped, symbolic links refused. [`examples/local/skills/pull-request-review`](../examples/local/skills/pull-request-review/SKILL.md) is one, mounted by the `reviewer` agent in [`examples/local/dekopon.yaml`](../examples/local/dekopon.yaml); its body tells the model to read the `references/risk-checklist.md` resource only when a diff touches authorization, credentials, or an external write.

Where a skill lives decides who mounts it. An agent's `spec.skills` names directories relative to the catalog file. The loader reads every one whole at catalog load, reports every directory that does not load alongside the catalog's other problems — one `dekopon validate` run diagnoses them all — and refuses two skills with one name for one agent, because a model could not tell two `read_skill` targets apart. `dekopond` binds the loaded skills to every route naming the agent and mounts them on every session of that route, shared rather than re-read, so a session never touches the filesystem. `dekopon-run prompt --skill <DIRECTORY>` and `session replay --skill <DIRECTORY>` mount the same format with no catalog and fail before any model call when a directory does not load. `dekopon describe agent` lists what an agent mounts by name, description, and resource count.

The model meets a skill in three steps, each paid for only when the model decides it needs it:

1. **The listing.** When at least one skill is mounted, a second system message follows the standing instructions. It begins `Skills mounted for this agent`, lists each skill as `- name: description`, and tells the model to call `read_skill` before doing work a skill covers. Nothing else of the skill is in the prompt.
2. **The instructions.** `read_skill` with `name` returns the skill's body, framed with its name and description and followed by the list of its resource files.
3. **A resource.** `read_skill` with `name` and `resource` returns one supporting file's text.

The listing is the trigger. The description is the one line the model matches a request against, which is why the format asks authors to write it as a "use when" sentence and why it is the only part that sits in every prompt. The body waits behind the tool because a tool result stays in the message vector and is re-sent on every later turn: a skill the model does not need for this request would otherwise cost its full length on every turn of every session. For the same reason a second read of the same instructions or resource within one session is answered with a one-line pointer at the earlier result rather than a second copy. A `SKILL.md` past 64 KiB is refused at load; a long checklist belongs in a resource the body names, read only when the case arises, which is what the example skill does with its risk checklist.

The listing is deterministic for one mounted set, and it sits with the instructions rather than with the request because it is agent-standing rather than request-scoped. On a `dekopond` route that keeps the leading prompt identical across sessions — the same instructions, the same listing, then what the conversation remembers and what the sender just said — so mounting a skill does not disturb the prompt-cache affinity [`inference.md`](inference.md#prompt-cache-key-lifecycle) describes. A `read_skill` result lands after that prefix, in the turn that read it.

An unknown skill name, or an unknown resource path, is a refusal the model reads and can recover from — it names the mounted skills, or the skill's resource files — and the session continues; `agent.skill.refused` records the reason (`unknown-skill` or `unknown-resource`) and never the name the model typed. Malformed arguments (not a JSON object, no `name`, an unexpected or mistyped field) end the session as a malformed call to any other tool does. Each successful read fires `agent.skill.read` with the operator-authored `skill.name`, the `skill.resource` path (empty for the body), the byte count, and whether it repeated an earlier read. Both events fire in either payload mode, because names and paths are operator-authored; the skill text reaches telemetry only inside the transcript events payload mode adds. `inspect_agent_config` shows the mounted skills as `skills: [{name, description, resources}]` — names and paths, never the text.

A skill is operator-authored text handed to the model, exactly as `instructions` is: it shapes an answer and grants nothing. Authority is only what the broker attests for this sender and this agent, so a skill that tells the model to reach for a capability it was not granted yields `command not found` like any other ungranted capability. The model can read every mounted skill in full, so nothing secret goes in one — no token, credential, or internal hostname — and `allowed-tools` in the front matter is carried through and rendered, never enforced.

### Tap the glass: `suggest_improvement`

An agent that hit a limit, reached for a capability it was never granted, or found its standing instructions wrong has learned something its operator would pay to know — and can otherwise say so only in chat, to a person who may not be the operator. `suggest_improvement` gives that observation a typed shape and a tagged telemetry record, so an operator can aggregate a month of sessions by category and target instead of reading transcripts.

**It is opt-in everywhere, and the opt-in is consent.** The tool is never offered unless the embedder asked for it: `dekopon-run prompt --suggestions`, `dekopon-run session replay --suggestions`, or `improvementSuggestions: true` on a `dekopond` route ([`dekopond.md`](dekopond.md#configuration)). The record carries model-authored text and is written in **either** payload mode — a suggestion nobody can read is not a suggestion — so offering the tool is what declares the telemetry sink in scope for that text. Nothing else widens with it: the record carries no chat text the gateway holds and no subject, only what the model chose to write into six bounded fields.

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

- `dekopon-run` prints each one to **standard error** after the answer, so standard output stays the model's text: `suggestion i/n [category, confidence confidence] target: summary`, then an `  evidence:` line and a `  proposal:` line. A replay carries them in its report as well.
- `dekopond` relays nothing to chat; the sender sees only the answer. The record exists in telemetry alone.
- An embedder of `dekopon-agent` receives them as `PromptOutcome.suggestions`, already written to telemetry by the time they arrive.

Reading them back is one query against the stream the exporters wrote to. OpenObserve stores the `audit.event` attribute as `audit_event`, folding every character outside letters, digits, and underscores:

```sql
SELECT * FROM "dekopon" WHERE audit_event = 'agent.improvement.suggested'
```

Group by `suggestion_category` and `suggestion_target` to see what a fleet keeps asking for; `trace_id` joins a suggestion to the session that made it, which `session show` then reconstructs. What comes back is advice from an untrusted model about its own configuration. A `capability` suggestion is a request for authority and gets the policy review any other would; an `instructions` or `skill` suggestion is a draft an operator turns into a catalog edit; and replay is how that edit is checked before it ships. Nothing reads these records but a person.

### Replay and evaluation

A session that already happened is the cheapest evaluation an operator can run: the prompt is real, the scripts the model wrote are known, and every script's output was recorded. `dekopon-run session` reads sessions back from the OpenObserve stream the runner and the gateway export to, and replays one against a model with the operator's change applied. [`run.md`](run.md#session-replay-and-evaluation) is the command reference — every flag, the receiver settings, the exact renderings and exit codes — and [`observability.md`](observability.md#reading-sessions-back) the record-level contract. This section is about what the loop can and cannot establish.

The source is the transcript. `accounting.model.turn` fires for every session in either payload mode, so `session list` covers everything a deployment ran; the transcript events (`agent.model.prompt`, `agent.model.answer`, `agent.tool.script`, `agent.tool.output`) exist only for sessions recorded with payload telemetry on — `--otel-telemetry-payloads true` on the runner, `telemetryPayloads: true` on the gateway — and `show` or `replay` of a session recorded without them fails, naming the trace and its accounted turn count. A deployment that wants to replay its sessions has therefore already put its prompts and script text into the sink, which is exactly the content [`observability.md`](observability.md#data-minimization) keeps out by default.

The three commands, in the order the loop uses them:

- `session list [--since 7d] [--limit 50] [--json]` groups accounting records by `trace_id`, newest first: trace, start time, turns, tokens, `answered` / `no-answer` / `failed`, and service. It is how a bad session is found.
- `session show (--trace-id ID | --from-file PATH) [--json]` reconstructs one session — its system messages, the earlier exchanges a persistent route replayed, the prompt, every turn with its scripts and their outputs, and the answer. `--json` prints the exact document `replay --from-file` reads back, so a recording can be kept beside the catalog, edited by hand, and replayed with no receiver in the loop.
- `session replay (--trace-id ID | --from-file PATH) --model MODEL …` puts the recorded conversation to a model again and answers every script from the recording. `--system TEXT` or `--system-file PATH` replaces every recorded system message; `--skill DIR` mounts skills and drops the recorded `Skills mounted for this agent` listing, so the replay lists exactly what the model can read; `--suggestions` offers `suggest_improvement` to the replayed model; `--provider COMPONENT` supplies read-only, import-free components for scripts the recording cannot answer.

**What replay establishes.** A script the replayed model writes is answered from the recording when its text exactly matches a recorded script whose output was recorded and not yet consumed, wherever that script sits — a model that reorders two independent scripts is still on the recorded trajectory — and the tool result is the recorded output and exit code. Up to that point the two sessions saw the same world, so a difference between them is the change under test and nothing else: the replayed model chose a different script, read a skill the recorded one did not have, or answered differently from the same evidence. A `read_skill` call is answered from the mounted skills rather than from the recording, so mounting a skill never diverges a replay by itself; the scripts the model writes after reading it are what the comparison shows.

**What replay honestly cannot do** is invent tool output. The first script the recording cannot answer is the divergence, and it is reported rather than papered over. Without `--provider` the script is answered `[replay stopped: the recorded session never ran this script and no live providers were supplied to run it]` and the replay ends there: the report's `divergence.handling` is `stopped`, the replayed answer is absent, and the exit code is `0`, because stopping at a divergence is the replay doing its job. With `--provider` the script runs live in direct mode — a component that can compute but never reach the network, with `curl` given no capability to assemble for — and the session continues with `handling` `live`; later scripts are still answered from the recording when they match. Either way the turns before the divergence are a faithful comparison and the turns after a live one are a new session, which the report says by carrying the divergence turn, the script, and the recorded scripts left unused. The exit code is `1` only when the replayed session failed for another reason — a model failure, `--max-steps` exhausted, a malformed tool call — and the comparison is still printed.

The report compares the two sessions at the level they can be compared: model turns, scripts index by index (`same`, `differs`, `recorded only`, `replayed only`), summed token usage, and both answers side by side. It does not score them. Whether the replayed session is better is the operator's call, made against the recorded one with every input visible; a team that wants a metric builds it over `--json`, where every field of the report is named.

**The edit-replay-compare loop**, end to end. The receiver is named by environment and the credential is read by name; its value never appears in an argument:

```console
export DEKOPON_OPENOBSERVE_URL=http://127.0.0.1:5080/api/default
# DEKOPON_OPENOBSERVE_AUTHORIZATION holds the complete Authorization header value.

# 1. Which sessions ran in the last day, and which failed or never answered?
dekopon-run session list --since 24h

# 2. Read the one that went wrong, then keep its recording.
dekopon-run session show --trace-id 4bf92f3577b34da6a3ce929d0e0e4736
dekopon-run session show --trace-id 4bf92f3577b34da6a3ce929d0e0e4736 --json > sessions/4bf92f35.json

# 3. Edit the standing instructions, or write the skill the session was missing.
$EDITOR instructions.md
$EDITOR skills/pull-request-review/SKILL.md

# 4. Replay the recording under the change: no receiver and no capability in the loop.
dekopon-run session replay --from-file sessions/4bf92f35.json \
  --model "$MODEL" \
  --system-file instructions.md \
  --skill skills/pull-request-review \
  --suggestions

# 5. Commit what worked to the agent's spec.instructions and spec.skills, then check it.
dekopon validate
dekopon describe agent reviewer
```

Step 4 repeats until the comparison reads the way the operator wants: the `differs` script is the one the new instruction was meant to change, the replayed answer says what the recorded one should have said, and the replayed model recorded no further suggestion about the thing just fixed. Step 5 is what makes the change real. `dekopond` reads the catalog at startup, so the edit reaches the gateway's sessions on its next start; `dekopon-run --system` and `--skill` take it immediately. Instructions and the skills listing are supplied to each session fresh and never stored in a remembered conversation, so nothing a persistent route holds has to be rewritten. A recording kept under version control beside the catalog is a regression test in the plainest sense: replay it after the next edit too.

## What is deliberately absent

Each of these is a decision, not a gap. Every artifact of the loop is either in the catalog, reviewed like the rest of it, or in the telemetry backend, retained and protected like the rest of it; nothing sits in a third place with a lifecycle of its own.

- **No durable suggestion store.** A suggestion lives in the telemetry record that carries it and, for the process that ran the session, in `PromptOutcome.suggestions` until that process exits. Nothing keeps a queue of pending suggestions, marks one applied, or reads yesterday's back into today's session. The telemetry backend already is the store, with the retention, access control, and full-text search an operator configured for the rest of the sink; a second store would be one more place model-authored text lives and one more thing to redact, back up, and expire.
- **No automatic prompt rewriting.** Nothing reads `agent.improvement.suggested` and edits `instructions` or writes a `SKILL.md`; nothing feeds a replay's result back into the catalog. Standing instructions and skills are operator-authored text that every later session on the route obeys, and letting one session's output edit them would make model text a channel into standing configuration with no review — the shape of thing the [non-negotiable invariants](design.md#non-negotiable-invariants) exist to refuse. Replay makes the human step cheap, which is the point: an operator who can check an edit in one command does not need the edit made for them.
- **No grader.** Replay reports `same` and `differs`, two answers, and two token totals; it does not ask a model whether the replayed answer is better. A model judging a model is one more untrusted opinion in a loop that is supposed to end at a person, and a score with no visible inputs is harder to defend in review than a diff with all of them. The `--json` report is the hook for a team that wants its own.
- **No cross-session memory.** A session's skill reads and suggestions are its own: `read_skill` returns a text once per session, the three-suggestion bound is per session, and nothing learned in one session reaches the next except through the operator's edit. The gateway's bounded conversation window and the optional durable chat-turn provider in [`inference.md`](inference.md#three-different-mechanisms) exist for a person's follow-up question, not for the agent improving itself, and neither carries a suggestion or a skill read.
- **No automatic replay.** `session replay` is a command an operator runs against a session they chose. No schedule replays a deployment's sessions and no gate blocks a catalog change on one.

## Related documents

- [`run.md`](run.md#mounting-skills) — `--skill`, `--suggestions`, and the complete `session list` / `show` / `replay` command contract with its renderings and exit codes.
- [`dekopond.md`](dekopond.md#sessions) — how a route mounts its agent's catalog skills and opts into `improvementSuggestions`, and why nothing a suggestion records reaches chat.
- [`catalog.md`](catalog.md#skills-are-directories-the-model-reads-on-demand) — the `spec.skills` field, the `SKILL.md` front matter, every bound, and what the loader refuses.
- [`observability.md`](observability.md#refusals-errors-and-outcomes) — `agent.skill.read`, `agent.skill.refused`, `agent.improvement.suggested`, and `agent.improvement.refused`; the transcript events a replay is rebuilt from; and the receiver contract behind `session list`.
- [`inference.md`](inference.md) — the prompt-cache prefix a stable skills listing preserves, and the conversation memory that is deliberately not an improvement mechanism.
- [`security-model.md`](security-model.md) — why operator-authored text handed to a model shapes answers and grants nothing.
