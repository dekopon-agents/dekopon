# PR3 approach: wake_reason

Status: revised after Astra note review r1; awaiting the coordinator's note-review
verdict. This commit changes only this note. Implementation has not started.

Base: `1d2d91961e037ebfd9aaea857f31e009878bf0d2`.
Branch: `codex/typed-events-pr3-1`.
Previous note head: `470e22b5ad9d9c152cae5d76480c81fc09c6589c`.
Stamped steering: xrl/agents `7a5ceaf641b3516be8cfe7b2ff0769ad6690f598`.
Its first line is `# Steering for GPT executors`.

The coordinator has now supplied DECISIONS-r4 sections 4 and 5 and PR1's
Non-goals, Contracts 1–11, Acceptance/Gates and Trust surface as message text.
They bind the plan below. No campaign-trail files were accessed. Follow the
brief's HELPER-RULES: generic User-Agent, no personal identifier in requests,
stop processes by their own PID, and delete any credential file written; this
work reads or writes no credentials.

## Conflicts and readings before implementation

| Sources in tension | Evidence at the base / supplied text | Reading to follow |
| --- | --- | --- |
| Design's `PrincipalName` and old session line hint versus code | `session.rs:31,1558,1565,2354` use `PrincipalId`; attribution reads the broker leg's principal | Use `PrincipalId`, only its display name or null; never derive it from message text or the external subject. |
| Section 4.1's Rust sketch renames variants but not fields; section 4.2 specifies camelCase keys | The sketch alone would emit `transport_kind` and `scheduled_by` | Keep `trigger` values kebab-case and explicitly set `rename_all_fields = "camelCase"`; section 4.2's JSON keys govern. |
| Section 4.2 shows prefixed IDs; the existing identifiers lack `Serialize` and display bare numbers | `agent/src/wake.rs:12–17` and `process/src/job.rs:6–23` | Keep typed `WakeId`/`JobId` fields and attach private, view-local `serialize_with` functions producing `w-42` / `j-9`. Do not change global identifier display or any protocol. |
| Section 4.2's illustrative webhook has two actor IDs; PR1 contract 2 and `GithubBatch` require one actor per batch | Example actors are 583231 and 9919; `GithubBatch::from_events` rejects mixed actors | Keep the same-actor invariant. Use two valid same-actor events in acceptance; expose the actual batch unchanged, not a copy of the illustrative fixture. |
| Section 4.2 says a webhook has one event/delivery; the adjacent example and section 4.4 require coalesced events | PR1 and the base support up to 16 events in arrival order | Each event keeps its own delivery ID; a coalesced turn returns every event, with no extra top-level delivery field. |
| F4's transport-kind wording mentions both Chat and Wake; the frozen enum and JSON give Wake only `wake` and `scheduledBy` | Section 4.1 enum and section 4.2 exact objects | Transport kind is a Chat field. Wake contains its typed ID and optional principal name only; add no transport, subject or conversation field. |
| Section 4.3's shorter facts list versus PR2's extra facts | `core/src/github.rs:739–773,802–805` include run ID, associated PR, conclusion and the report-only instruction | Retain all four extras, as the brief expressly allows. Keep the report-only instruction before the exact new closing line, which appears once at the very end. |
| Section 5 row 3's crate summary versus the current renderer's home | `GithubEvent::note` and `GithubBatch::note` live in core, called by gateway `run_admitted` | Adapt the sole existing core renderer under the brief's explicit permission; do not create a gateway copy. Treat core's edit as making SDK/provider-workspace gates applicable. |
| Brief's unfiltered authorization grep versus checked-in tests | Production calls are `broker/src/lib.rs:2199–2200` in `evaluate`; the grep also finds `brokerd/tests/integration/examples.rs:168,177,204,220,230` | Preserve the production invariant and inherited tests; report the literal mismatch as Astra's inherited follow-up, without claiming the raw grep prints only two lines. |
| Existing meta module comment versus the required principal-bearing view there | `agent/src/meta.rs:1–3` describes the module as having no principal field | Put `WakeReasonView` in `dekopon-agent::meta` as required; scope the existing constraint to `AgentConfigView`, which stays identity-free. |
| Generic final-PR/build instructions versus this delivery checkpoint | The coordinator explicitly requests a revised note, new commit and push, then ending the turn without building | Change only `approach-note.md` now; no feature build, implementation, PR or reviewer launch. Delete the note in the final implementation commit after note approval. |
| Repository package examples omit `--no-verify`; the supplied kickoff fixes that gate's command | PR1 Acceptance/Gates specifies `cargo package --workspace --locked --no-verify` on a README edit | Run that exact kickoff gate for implementation; do not substitute the public-crate release list or silently waive a failure. |

The security documentation's case statement is a required correction, not a
reason to change normalization: `docs/security-model.md:73` says facts keep
GitHub's case, while repository names are lowercase. Add the exact exception
"except repository names, which are lowercase". F2 (`github.denier=facts` on a
non-GitHub denial) and F3 (`with_event_string_array` naming) remain follow-ups.

## View and tool

Define the view in `dekopon-agent::meta` and pass an optional borrowed view through
exactly `SessionInputs::wake_reason(&WakeReasonView)`, then `SessionExtensions`,
into the existing prompt loop. Keep `AgentConfigView` separate. The planned
serialization shape is:

```rust
#[derive(Serialize)]
#[serde(tag = "trigger", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum WakeReasonView {
    Chat {
        transport_kind: ChatTransportKind,
        principal: Option<PrincipalId>,
        admission: Admission,
        steers: u32,
    },
    Webhook {
        source: WebhookSource,
        events: Vec<GithubEvent>,
    },
    Wake {
        #[serde(serialize_with = "serialize_wake_id")]
        wake: WakeId,
        scheduled_by: Option<PrincipalId>,
    },
    Job {
        #[serde(serialize_with = "serialize_job_id")]
        job: JobId,
    },
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Admission { New, FollowUp }

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookSource { Github }
```

Reuse `ChatTransportKind` from `dekopon-broker-protocol`, `PrincipalId` and
`GithubEvent` from core, `WakeId` from `dekopon-agent::wake`, and `JobId` from
`dekopon-process`; these are existing dependencies. Do not add `NonePerson`:
s6-digest owns its first real consumer. No generic identity/conversation field,
prose field, new Cargo dependency/feature, protocol field or config key is added.

Both private serializer functions take a typed ID and a serde serializer and
serialize a string using `Serializer::collect_str` with `format_args!("w-{id}")`
or `format_args!("j-{id}")`. Existing `Display` supplies the numeric portion.
This keeps numeric IDs typed inside the view without requiring global `Serialize`
implementations or changing existing wake/job arguments, storage or wire forms.
`PrincipalId` already serializes transparently to its name. Neither optional
principal field has `skip_serializing_if`: absent values are explicit JSON null.

Exact non-webhook shapes (key order is immaterial):

```json
{ "trigger": "chat", "transportKind": "slack", "principal": "ana", "admission": "followUp", "steers": 1 }
{ "trigger": "chat", "transportKind": "local", "principal": null, "admission": "new", "steers": 0 }
{ "trigger": "wake", "wake": "w-42", "scheduledBy": "ana" }
{ "trigger": "wake", "wake": "w-42", "scheduledBy": null }
{ "trigger": "job", "job": "j-9" }
```

The webhook object has exactly `trigger: "webhook"`, `source: "github"` and
`events`, where `events` is the serde array of the actual `GithubEvent` values.
There is no extra principal, admission, steer count or delivery field on it.

Construct the owned view in gateway `session_turn`, after the existing broker
leg has returned its principal and before the blocking prompt task is launched.
The task owns that view and borrows it for `SessionInputs::wake_reason` around
`session.rs:2967`, outside the `inspect_agent_config` branch. Thus every gateway
model session receives a view without another broker call. TUI and console
embedders that supply no view keep the tool absent.

| `MessageId` | Construction from existing data |
| --- | --- |
| `Native` or `Follow` | `Chat`: authenticated `message.transport_kind`, the same `leg.principal().cloned()` used by `attributed_prompt`, the admission kind carried below, and initial `steers: 0`. |
| `Github(batch)` | `Webhook`: `WebhookSource::Github` and `batch.events().to_vec()` in arrival order. `chat_claim_for` at `session.rs:3697–3703` already copies the same batch into `ChatScopeClaim.github`. No payload reparse or independent fact projection. |
| `Wake { id, .. }` | `Wake`: the existing typed `WakeId` and `scheduled_by` from the existing broker-mapped principal. Do not copy the wake notice text, anchor subject or conversation. |
| `Job { id, .. }` | `Job`: the existing typed `JobId` only. Do not expose agent, job text or conversation. |

**Admission reaches construction explicitly.** Add an `Admission` field to the
existing internal `SessionAdmission` at `session.rs:846`, initialize it to `New`
when `SessionGate::admit` creates a fresh admitted turn at line 707, and set it to
`FollowUp` in `next_or_release_with` when it selects the next queued turn. That
covers queued native messages and leftover steers folded into native turns;
`MessageId::Native` need not change. Pass this small value through `run_admitted`
to `session` and `session_turn` alongside the existing cancellation input, then
into Chat construction. No `InboundMessage`, broker claim or queue format changes.
Admission names the actual admission path: even a `MessageId::Follow` admitted
fresh is `new`; any dequeued Chat is `followUp`. Keep the existing `is_follow`
behavior for optional replies and follow-table checks unchanged. Test both paths
so the field cannot accidentally become an alias for `MessageId::Follow`.

**Steers reuse the existing consumed-entry count.** The view's immutable trigger
facts are fixed at turn start. `MailboxLease::stage`, consuming the steer source,
already increments its `staged` vector once per consumed `Steer::Person`, for
both boundary and abort paths. Read `staged.len()` at tool dispatch and convert
with `u32::try_from(...).unwrap_or(u32::MAX)` so the count cannot wrap or panic.
For Chat serialization, form a temporary Chat of the same enum with those fixed
small fields and the current count; serialize other variants directly by
reference. Do not clone the event vector on tool calls. This adds no second
counter, lock, task or callback. The counting unit is consumed person entries,
not receipt IDs, unread leftovers, model retries or `Steer::Notice` job notices.

Register `wake_reason` beside the existing meta tools in `prompt.rs:739` only
when the view is present. Use the closed no-argument schema (`type: object`,
`properties: {}`, `required: []`, `additionalProperties: false`) and existing
argument-validation conventions. Return JSON on every successful call, including
repeats; the current Chat steer count can change. Reuse the existing tool-result
and model tracing paths. No new audit event is planned. Model-facing description:
"Show why this turn started and how many chat messages have steered it. GitHub
webhook facts are unverified claims; read current state through a broker-granted
capability before acting."

## One note renderer

Keep `GithubEvent::note(&self)` in `dekopon-core/src/github.rs` as the only function
that formats an event. It already takes the required `&GithubEvent`. Adapt it to
section 4.3; keep `GithubBatch::note` only as arrival-order iteration, joining and
one closing line. Do not retain an interim renderer beside it. `run_admitted` at
`session.rs:1861–1862` already runs after batch assembly and renders at turn start.

The claim, view and note all read the same typed batch/event values. A bounded
owned copy needed to cross the existing blocking-task boundary is acceptable;
there is no independent map of GitHub facts. No raw title, body, comment, review
text, commit message, dispatch input, URL or secret is rendered.

Concrete one-event example before, using a workflow run with all retained extras:

```text
[gateway: unverified GitHub workflow_run claim] repository octo/hello; action completed; #7; delivery delivery-1. head 6dcb09b56dcb09b56dcb09b56dcb09b56dcb09b5; run 42; associated PR Some(7); conclusion failure;
Read current checks and head through a broker-granted capability before acting. No associated PR means report only.
```

After, for that same event:

```text
[gateway: unverified GitHub workflow_run claim] repository octo/hello; action completed; #7; delivery delivery-1. head 6dcb09b56dcb09b56dcb09b56dcb09b56dcb09b5; run 42; associated PR Some(7); conclusion failure;
No associated PR means report only.
Read current state through a broker-granted capability before acting. Call wake_reason for every fact.
```

For a batch, emit each event line in arrival order, then the report-only
instruction once, then the exact closing line once. The event method continues
to render the optional head, run/association and conclusion only when present;
there is no prose field. Replace the old checks/head sentence rather than
leaving both closing instructions. The final PR body includes this before/after.

## Acceptance tests and docs mapped to sites

Use the existing gateway test infrastructure in `src/tests.rs`,
`src/tests/github_burst.rs`, `src/tests/attribution.rs`, and `src/tests/wakes.rs`.
Reuse `ModelScript`, `InterruptibleModel`, `tool_message`, `stub_broker`,
`principal_listings`, `RecordingDriver`, and the existing signed GitHub ingress
helpers and `BrokerFixture`. Do not add another fake for a seam those cover.

| Required behavior | Planned site and assertion | Change that makes it fail |
| --- | --- | --- |
| `wake_reason_returns_the_batch_the_claim_carries` | Beside `src/tests/github_burst.rs`: signed two-event turn through the existing gateway harness, scripted tool call, deserialize the result's `events` and the captured actual frame's `scope.github` into `Vec<GithubEvent>`, and compare typed values and arrival order. The fixture uses two valid events from one actor. If setup must be shared, extract the existing fixture helper instead of duplicating it. | Remove tool wiring, expose only the lead, reorder events or serialize a separate fact projection. |
| `wake_reason_never_names_a_subject_or_conversation` | Gateway tests using the existing Slack follow/attribution and wake helpers: invoke the tool on a dequeued Slack native follow-up and a wake, parse `Value`s and compare complete objects (including `followUp`, `w-42` and `scheduledBy`) with a known principal and distinct subject/conversation sentinels. Also cover null principal serialization. Assert absence structurally, not via log text. | Add a forbidden field, source the principal from the subject, or expose an anchor/conversation object. |
| `wake_reason_is_offered_on_every_gateway_session` | Beside the existing `a_route_that_withholds_self_inspection_offers_no_such_tool` test at `src/tests.rs:17060`: exercise both `inspect_agent_config` values and Chat/Webhook/Wake/Job triggers using the existing model's captured tool definitions. Add agent-loop coverage that an absent view omits the tool. | Gate it on inspection, omit any gateway-trigger path, or offer it to an embedder without a view. |
| New versus dequeued Chat admission | Reuse the gateway queue/attribution helpers: a freshly admitted native Chat is `new`, then a native Chat dequeued through `next_or_release_with` is `followUp`. Cover a freshly admitted `MessageId::Follow` as `new` without changing its existing follow behavior. | Infer admission only from `MessageId::Follow`, or lose the kind between admission and view construction. |
| Typed ID serialization | Agent meta-view tests compare parsed Wake/Job JSON against `w-42` / `j-9`; preserve the original identifier `Display` implementations. | Emit bare numbers, omit prefixes or globally alter identifier display. |
| A steered turn's `steers` | Existing interruptible model/attribution scenario: consume a person steer, call again, parse the result and assert the updated count; queued leftovers and notices do not increment it, and both boundary and abort paths use the same consumed-entry count. | Freeze the count at turn start, count a model retry or count only gateway admission. |
| Note matches frozen text | Update the existing core note coverage to compare the required closing line and retained workflow facts; exercise arrival-order rendering through the two-event gateway scenario. | Retain a parallel formatter, lose the extras or alter the closing line. |

Before implementing, stage the new acceptance tests against the stamped base and
record their observed failure. They have not been added or run in this note-only
phase; absence of the tool is not being reported as an observed failing test.

| Required document | Edit site and content |
| --- | --- |
| `docs/gatewayd.md` | Beside the `suggest_improvement` session sentence near line 1324: every gateway route offers the no-argument tool, independent of `inspectAgentConfig`. |
| `docs/architecture.md` | The `dekopon-agent` ownership/meta-tools list near line 39. |
| `docs/design.md` | The component-boundaries `dekopon-agent` row at line 158. |
| `docs/security-model.md` | Typed GitHub facts and principal-only inspection: no new authority or broker call; no subject or conversation fields on Chat/Wake. Include F1's lowercase-repository exception. |
| `crates/dekopon-agent/README.md` | `SessionInputs`/meta-tool API and presence rule, JSON trigger variants, current steer count and no-view omission. |

No WIT, wire-frame, `context.github`, Cedar, policy-digest, config-key, rpi-homelab
or live-system change is planned. No merge, tag, release or deployment is allowed.

## Validation and delivery after note approval

Observed note-only validation on this revision before commit, with only
`approach-note.md` changed atop `470e22b5`:

| Check | Result |
| --- | --- |
| `git diff --check` | Passed. |
| `python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md` | Passed; 49 Markdown files. This existing command does not scan the root approach note. |
| Exact CI audit-event documentation step, extracted unchanged from `.github/workflows/ci.yml` and executed by Bash | Passed; 45 audit event names. |
| Authorization call-site grep | Baseline observation from the previous note: two production calls plus five inherited test calls. Source is unchanged in this revision; the grep was not rerun. |
| All feature gates and acceptance tests listed below | Not run in the note-only phase; implementation awaits coordinator note approval. All provider-workspace gates below are also not run. |

No feature build is authorized before the coordinator's note review passes.

After approval, run the complete supplied kickoff gates plus applicable
AGENTS.md/CI checks. The build/test commands below are planned, not run in this
note round; the whitespace and documentation checks are recorded above:

| Gate | Command or exact source |
| --- | --- |
| Whitespace | `git diff --check`. |
| Workspace format | `cargo fmt --all --check`. |
| Workspace clippy | `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`. |
| Release compile | `cargo check --release --locked -p dekopon-brokerd -p dekopon-gatewayd`; include `-p dekopon-console` as current CI does. |
| Unused dependencies | `cargo machete`. |
| Both daemon normal-dependency boundaries | Run both exact `cargo tree --locked ... --edges normal --prefix none` rejection steps from ci.yml, plus `python3 .github/scripts/test_daemon_dependency_gates.py`. |
| SDK Wasm | `cargo check --locked -p dekopon-provider-sdk --target wasm32-unknown-unknown`. Applicable because the shared renderer edit touches core. |
| No exporter | `cargo clippy --locked -p dekopon-telemetry --all-targets -- -D warnings`, then `cargo check --locked -p dekopon-http-host -p dekopon-agent`. |
| Rustdoc | `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked`. |
| External fixtures, test compile, execution and doctests | `ci/fetch-external-provider-components.sh examples/providers`, then `cargo test --workspace --all-features --locked --no-run`, then CI's `DEKOPON_TEST_BROKERD`, `DEKOPON_TEST_PROBE_WASM` and `DEKOPON_TEST_ASSET_WASM` paths with `cargo test --workspace --all-features --locked --lib --bins --tests`, then `cargo test --workspace --all-features --locked --doc`. |
| Dependency policy | `cargo deny --all-features check`. |
| Package | `cargo package --workspace --locked --no-verify` from a clean tree, required because the agent README changes. |
| Documentation | `python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md` and the exact ci.yml audit-event documentation step. |
| Repository shell checks | Run ci.yml's shellcheck command over its listed scripts, as AGENTS.md requires CI's applicable checks; no shell edits are planned. |
| Required feature scenarios | Run all three named acceptance tests, the steer-count and admission cases, and the two-event GitHub scenario through the existing gateway harness. |
| Remote validation | Exact-head CI plus coordinator-launched Astra and Claude implementation reviews, each building the head and running the three named tests and two-event scenario. |

**Separate provider workspaces are included, not covered by root commands.**
The core renderer edit makes all seven affected SDK consumers applicable:
`cli-probe`, `clock-probe`, `clock-raw-probe`, `http-probe`, `http-raw-probe`,
`memory-reservation-probe`, and `storage-probe`. For each one's manifest, run:

```console
cargo fmt --manifest-path examples/providers/<PROVIDER>/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml
cargo check --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml --target wasm32-unknown-unknown
```

For each checked probe, also run its pinned `build.sh`, byte-compare the rebuilt
component against the checked artifact as CI does, run `wasm-tools validate`, and
inspect `wasm-tools component wit --json` against the exact import table in
`docs/development.md` under Provider example workspaces. All seven must export
`run-command` first, then `describe` and `invoke`, with no WASI imports. Use the
pinned compiler and wasm-tools versions; never hand-edit a component. Root tests
and provider checks both use `--locked`; any required lockfile regeneration uses
Cargo before resuming locked validation.

Name every unavailable or failed gate, its reason, and tested head. The two
root-only permission-test limitations remain exactly those in
`docs/development.md`; report them instead of editing unrelated tests. ARM64
host/optimized validation requires its matching host or exact-head CI. PR2's
body reports an inherited private-console packaging resolution failure, not
reproduced in this note phase; run the literal package gate and report its
actual result rather than replacing it with the public-crate release list.

After the note passes, implement and verify, delete this note in the final commit,
push without amending any pushed commit, and open the single ready PR only with
passing required gates and exact-head CI. Title:
`feat(agent): the wake_reason tool`. User-visible commit trailer, one physical
line:

```text
Changelog: Added: the wake_reason tool returns why a turn started.
```

Use the repository PR template, the dispatched Trust surface verbatim, the
before/after note, a validation table naming tested heads and toolchain, all
choices below and any further choices. The coordinator launches Astra and Claude
implementation reviews; each builds the head, runs the three tests and the
two-event gateway scenario. The coordinator alone owns the post-deploy passive
24-hour check on tool definitions and error-free calls.

## Choices I made

- Use section 4.2's camelCase field names explicitly in serde while keeping kebab-case trigger tags; the section 4.1 sketch alone would not rename fields.
- Use the code's `PrincipalId` for the design's `PrincipalName`, sourced only from the existing broker leg, and serialize absent names as null.
- Keep typed Wake/Job IDs and implement private view-local prefixed string serializers; do not change their global display or protocols.
- Keep admission on the existing `SessionAdmission`: fresh admission is `new`, every dequeued Chat is `followUp`, independently of `MessageId::Follow`.
- Reuse consumed `Steer::Person` entries in `MailboxLease.staged` for the current saturating u32 count, excluding job notices and unread leftovers; serialize a temporary Chat of the same enum without cloning webhook events.
- Construct the view after existing gateway attribution and supply it unconditionally through `SessionInputs::wake_reason`; use the model-facing description quoted above, with no inspection opt-in or new broker call.
- Keep the existing core event-note method as the only renderer and the batch method only as assembly, retaining run ID, associated PR, conclusion and the report-only instruction before the exact closing line.
- Follow PR1's same-actor batch invariant rather than the mixed actors in section 4.2's illustrative JSON; every returned event keeps its own delivery ID.
- Place acceptance in the existing gateway harness and reuse its model, broker and signed-ingress helpers; cover native queued admission explicitly and deserialize compared event arrays to the core type.
- Preserve the authorization grep's inherited test matches and report the literal mismatch, without changing unrelated policy tests.
