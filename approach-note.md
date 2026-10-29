# PR3 approach: wake_reason

Status: blocked approach note, not an implementation or a passed design review.

Base: `1d2d91961e037ebfd9aaea857f31e009878bf0d2`.
Branch: `codex/typed-events-pr3-1`.
Stamped steering: xrl/agents `7a5ceaf641b3516be8cfe7b2ff0769ad6690f598`.
Its first line is `# Steering for GPT executors`.

## Missing binding inputs

The dispatch says the coordinator supplies DECISIONS-r4.md section 4, section 5
row 3, and the PR1 brief's Contracts 1–11, Non-goals and Gates as message text.
Those excerpts were not attached. The exact section 4.2 JSON and section 4.3
closing line therefore cannot be checked or reconstructed from authority.
The full PR1 Trust surface is also requested, together with any HELPER-RULES.md
requirements beyond the three rules quoted in this brief. The merged PR1 body's
Trust surface is available as corroboration, but is not a substitute for the
missing binding dispatch. No campaign-trail files were accessed.

The sections below map the supplied brief to the stamped code. Schema details,
exhaustive design conflicts, and final note text remain blocked until the missing
text arrives. No field, spelling, trigger distinction or note closing sentence is
being invented to fill that gap. This note needs completion before an Astra note
review can approve implementation.

## Conflicts and readings before implementation

| Sources in tension | Evidence at the base | Reading to follow |
| --- | --- | --- |
| Design's `PrincipalName` and old session line hint versus code | `session.rs:31,1558,1565,2354` use `PrincipalId`; attribution reads the broker leg's principal | Use `PrincipalId`, only its display name or null; never derive it from message text or the external subject. |
| Section 4.3's narrower note list, as described by this brief, versus PR2's extra facts | `core/src/github.rs:739–773,802–805` include run ID, associated PR, conclusion and the report-only instruction | Retain all four extras, as the brief's default allows; append the exact frozen closing line after the retained report-only instruction. Exact before/after awaits section 4.3. |
| Brief's unfiltered authorization grep versus checked-in tests | Production calls are `broker/src/lib.rs:2199–2200` in `evaluate`; the same grep also finds `brokerd/tests/integration/examples.rs:168,177,204,220,230` | Preserve the production invariant and these inherited tests; report the literal grep mismatch, without claiming that it prints only two lines. No authorization changes are planned. |
| Existing meta module comment versus adding a principal-bearing view there | `agent/src/meta.rs:1–3` describes the module as having no principal field; it currently holds `AgentConfigView` | Keep `AgentConfigView` identity-free; scope that existing constraint to its actual type or place the new view in a sibling module if the frozen text requires it. Do not treat this comment as authority to omit the new view's permitted principal. |
| Delivery's approach-note checkpoint versus generic steering's final-PR reporting | This brief explicitly requires a pushed note, ending the turn, and a coordinator-launched note review before implementation | Deliver this note first. Do not build the feature, open the final PR, or launch the coordinator's review here. Delete the note in the final implementation commit after review. |
| Missing frozen excerpts versus instruction to enumerate every conflict | Only the PR3 brief and stamped repository are available | Record the observed differences and stop dependent work; an exhaustive brief/design/base comparison remains unverified. |

The security documentation's case statement is a required correction, not a
reason to change normalization: `docs/security-model.md:73` says facts keep
GitHub's case, while repository names are lowercase. Add the exact exception
"except repository names, which are lowercase". F2 (`github.denier=facts` on a
non-GitHub denial) and F3 (`with_event_string_array` naming) remain follow-ups.

## View and tool

Put the required `WakeReasonView` with the agent's meta views, keeping it separate
from `AgentConfigView`. Mirror `SessionInputs::with_agent_config` with an optional
borrowed wake-reason input, threaded through `SessionExtensions` into the existing
prompt loop. The only new cross-crate API is the one directly required for this
view and its input; no new dependency, protocol field or configuration key is
planned.

Construct the view in gateway `session_turn`, after the existing broker leg has
returned its principal and before the blocking prompt task is launched. Supply it
unconditionally when creating `SessionInputs` around `session.rs:2967`, outside
the `inspect_agent_config` branch. Do not ask the broker again when serving the
tool. TUI and console callers that supply no view keep the tool absent.

Use a closed enum and typed fields, with no external-subject or conversation type
in its `Chat` or `Wake` variants. Do not add `NonePerson`. The precise serde tag,
field names, optional/null rules, transport vocabulary and Wake/Job payloads must
come directly from the missing section 4.2.

| Trigger at the existing seam | Construction from existing data |
| --- | --- |
| Native chat and follow messages | Transport kind from authenticated inbound metadata, optional principal from the same broker leg used by `attributed_prompt`, and the frozen `new`/`followUp` distinction. `MessageId::Follow` exists; whether that alone defines `followUp` must be checked against the missing contract. |
| GitHub webhook | Match `MessageId::Github(batch)` and expose its bounded typed events in arrival order, with `source: github`. `chat_claim_for` at `session.rs:3697–3703` already copies this same batch to `ChatScopeClaim.github`; no payload reparse or second fact extraction. |
| Scheduled wake | Match `MessageId::Wake` and use only section 4.2's allowed wake fields plus the permitted broker principal name. Do not copy the wake anchor's subject or conversation. |
| Job notice turn | Match `MessageId::Job`; populate only section 4.2's Job fields from existing typed job metadata. No provider invoke or broker request is caused by inspection. |

For `steers`, use the prompt loop's existing `MailboxLease.staged.len()` when a
call is served. It records consumed `Steer::Person` entries and already works for
both boundary and abort paths. Proposed counting unit: consumed person entries,
not raw receipt IDs, queued leftovers, model retries or `Steer::Notice` job
notices. This requires no second counter, lock, task or gateway callback. Confirm
that this reading agrees with section 4.2 before implementation.

Register `wake_reason` beside existing meta tools in `prompt.rs:739` only when the
view is present. Use the existing closed no-argument object schema pattern
(`properties: {}`, `required: []`, `additionalProperties: false`) and argument
validation conventions. Return JSON on every successful call, including repeats;
the current steer count can change. Reuse the existing tool-result and model
tracing paths. No new audit event is planned.

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

Current note template, before:

```text
[gateway: unverified GitHub <event> claim] repository <repo>; action <action-or-none>; <#number-or-repository>; delivery <delivery>. head <sha>; run <id>; associated PR <Some(number)-or-None>; conclusion <conclusion>;
Read current checks and head through a broker-granted capability before acting. No associated PR means report only.
```

Head, run/association and conclusion fragments appear only when present in the
current typed event. The PR body must show a concrete before/after example once
the missing exact closing line has been supplied. The after text is deliberately
not fabricated in this blocked note.

## Acceptance tests and docs mapped to sites

Use the existing gateway test infrastructure in `src/tests.rs`,
`src/tests/github_burst.rs`, `src/tests/attribution.rs`, and `src/tests/wakes.rs`.
Reuse `ModelScript`, `InterruptibleModel`, `tool_message`, `stub_broker`,
`principal_listings`, `RecordingDriver`, and the existing signed GitHub ingress
helpers and `BrokerFixture`. Do not add another fake for a seam those cover.

| Required behavior | Planned site and assertion | Change that makes it fail |
| --- | --- | --- |
| `wake_reason_returns_the_batch_the_claim_carries` | Beside `src/tests/github_burst.rs`: signed two-event turn through the existing gateway harness, scripted tool call, parse its result and the captured actual frame's `scope.github`, compare event values and arrival order. If setup must be shared, extract the existing fixture helper instead of duplicating it. | Remove tool wiring, expose only the lead, reorder events or serialize a separate fact projection. |
| `wake_reason_never_names_a_subject_or_conversation` | Gateway tests using the existing Slack follow/attribution and wake helpers: invoke the tool on a Slack follow-up and a wake, parse `Value`s and compare complete frozen objects with a known principal and distinct subject/conversation sentinels. Assert absence structurally, not via log text. | Add a forbidden field, source the principal from the subject, or expose an anchor/conversation object. |
| `wake_reason_is_offered_on_every_gateway_session` | Beside the existing `a_route_that_withholds_self_inspection_offers_no_such_tool` test at `src/tests.rs:17060`: exercise both `inspect_agent_config` values and Chat/Webhook/Wake/Job triggers using the existing model's captured tool definitions. Add agent-loop coverage that an absent view omits the tool. | Gate it on inspection, omit any gateway-trigger path, or offer it to an embedder without a view. |
| A steered turn's `steers` | Existing interruptible model/attribution scenario: consume a person steer, call again, parse the result and assert the updated count; queued leftovers and notices follow the documented counting unit. | Freeze the count at turn start, count a model retry or count only gateway admission. |
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

Observed note-only validation on the staged approach-note tree rooted at the
stamped base:

| Check | Result |
| --- | --- |
| `git diff --cached --check` | Passed. |
| `python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md` | Passed; 49 Markdown files. This existing command does not scan the root approach note. |
| Exact CI audit-event documentation step, extracted unchanged from `.github/workflows/ci.yml` and executed by Bash | Passed; 45 audit event names. |
| Authorization call-site grep | Observed the two production calls plus five inherited integration-test calls described above. |
| All feature gates and acceptance tests listed below | Not run in the note-only phase; implementation awaits the missing contract and coordinator note review. |

No feature build is authorized before the coordinator's note review passes.

For implementation, preserve all listed gates: fmt; workspace all-target,
all-feature locked clippy with `-D warnings`; release check of both daemons (and
the console included by current CI); machete; both normal-dependency boundary
checks and their Python gate tests; the wasm SDK check; CI's two no-exporter
commands; warnings-denied workspace rustdoc; fetch external provider components;
workspace all-feature locked test compilation, `--lib --bins --tests`, and
`--doc`; cargo deny; cargo package because the agent README changes; docs
duplicates; and the CI audit-event step. Resolve the exact inherited PR1 gate list
from the missing dispatch before claiming completeness. Run the literal package
gate rather than substituting the release-public-crate list; PR2's body reports
an inherited private-console dependency-resolution failure, which has not been
reproduced here. Name every unrun or failing gate and its reason.

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

- Preserve an explicitly blocked approach note instead of inventing missing frozen contract text; no implementation is started.
- Plan the view beside agent meta views, as a separate type from the identity-free agent configuration view.
- Plan gateway construction after existing broker attribution and unconditional injection into its session inputs, with no new broker call.
- Propose consumed `Steer::Person` entries in the existing mailbox lease as the counting unit, excluding job notices and unread leftovers; check against the missing contract before coding.
- Keep the existing core event-note method as the one renderer and the batch method only as assembly, retaining all four PR2 workflow extras.
- Place behavioral acceptance in the existing gateway harness and reuse its model, broker and signed-ingress helpers.
- Report the authorization grep's inherited test matches rather than changing unrelated policy tests or claiming the literal baseline gate passes.
