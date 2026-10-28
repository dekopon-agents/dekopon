# Typed GitHub facts: PR2 approach

Pinned steering: “# Steering for GPT executors” (xrl/agents, 7a5ceaf641b3516be8cfe7b2ff0769ad6690f598).

This is the pre-code review checkpoint for diaphanous-dilettantes, step 09-github-events/events/typed-events-pr2. Base: 46d54e8f7d36ffde50c607cc3ff178c67a2c552d (#814); PR1 merge 7092e5b6 is its ancestor. Branch: codex/typed-events-pr2-1. The fetched fixture branch is exactly d296d95b52cfafe576bb54c5e1b4c683c9f37202. Only this note is implemented; building waits for the coordinator-launched Astra approach review to pass.

## Types, ownership and flow

- dekopon-core::github owns GithubEvent, the existing closed GithubEventName, GithubThread, GithubBatch (1–16 in arrival order), Target and the §3.3 leaf records. Validated newtypes enforce IDs/numbers within positive i64, delivery GUIDs, SHA, canonical repository names, logins, GithubWord, ref names, bounded character-counted names and UnixSeconds. Deserialization uses the same constructors; wire decoding cannot bypass bounds.
- Every member of shared run/check/alert/deployment/gitRef records is optional in Rust and Cedar; each family parser enforces its own required fields. Bound labels at 100, assignees at 10 and associated PRs at 16; preserve the under-512-KiB field budget within the existing 2-MiB frame ceiling.
- Extend the existing core github.rs; move its body-based free thread() to GithubEvent::thread() and delete the free function and associated_thread(). Thread derivation uses the typed number or the repository-filtered run/check PR set; the existing action-sensitive local parent/blocked-issue payload handling stays at the parser boundary.
- gatewayd's existing GitHub transport reads signed serde_json::Value bodies by pointer, constructs one GithubEvent per family, and derives routing from that event. Replace GithubEntry, the transport-local batch and parse's tuple with core facts; keep the existing session admission, queue and action-filter owners.
- A renderer over &GithubEvent replaces the transport's hand-built entry note. The note, claim and subsequent wake_reason consumer use these same facts; no second payload summary or prose field is introduced.
- broker-protocol adds ChatScopeClaim.github: Option<GithubBatch> with default and skip_serializing_if = Option::is_none. gatewayd attaches it and first-event DeliveryIdentity::Github only to GitHub message claims, including coalesced batches; wake claims omit it. In-tree claim constructors receive github: None where appropriate.
- dekopon-policy extends schema_json() with the complete optional github record in capability, prompt and secret contexts. Assemble the existing routing context (including via and agent), the action-specific attributes (effect/risk for capabilities; capability/provider/sink for secrets), and "github": serialized_event into one complete JSON context before schema-aware Context::from_json_value conversion. Use this assembly for both the full event and the event with authorSet omitted. Afterwards, merge the existing HTTP request context through its current path: that dynamic record intentionally remains outside the generated action schema. Preserve context.request.body and policy digest inputs.
- broker resolve_context validates scope coordinates; its existing evaluate function owns the all-events, full/stripped and asker/space intersection. Existing decision aggregation retains policy IDs and evaluation errors, with deterministic first-denying delivery and facts/author-set phase for telemetry.
- Update dekopon-policy README, dekopon-brokerd README's Cedar table, docs/security-model.md and docs/observability.md with the implemented behavior and bounded structured fields.

## Contracts and invariants (1–11)

1. One public GithubEvent::thread() returns canonical GithubThread; gateway and broker call it. Foreign issue/PR numbers are omitted before threading and serialization; no parallel thread extractor survives.
2. Keep enqueue coalescing under the existing in_flight lock/follow-up deque: at most 16 events from one actor.id, never steering or interrupting. Keep mailbox 100 and interrupt cap 8.
3. Preserve PR1's Reserved/Committed GUID lifecycle and admission acknowledgement: only admitted, queued or ignored gets 200; busy, timeout and closed channel get 503 within WEBHOOK_REQUEST_TIMEOUT.
4. Keep classification ahead of repository requirements, all four ignore reasons, the closed family inventory, and the existing pull_request/workflow_run action filters; the parser itself remains usable for every fixture action.
5. Omit github on non-GitHub frames and wakes. resolve_context returns attestation-denied for wrong kind/trigger, foreign repository.fullName, event/thread mismatch or missing/mismatched first delivery. Target and every leaf refuse unknown authored keys; GithubEvent has no deny_unknown_fields beside flatten.
6. All events must allow with complete facts and with authorSet omitted; any evaluation error denies. Apply the same intersection for both principals inside evaluate; policy.authorize/authorize_http gain no production call site and digests remain unchanged.
7. Required, guaranteed facts alone can cause Malformed; optional bad facts disappear and bad set elements are dropped. Unknown payload fields are ignored, including nested fields.
8. The serde event is the Cedar value and the renderer input. Strict schema validation across every family pins the only intentional Rust/Cedar shape mirror.
9. Reuse the closed GithubEventName inventory; issue_relates_to, discussions and future unknown headers remain ignored as unknown-event.
10. Preserve gateway.admission's github.deliveries. Add index-aligned broker.decision github.delivery/event/actor.id arrays, event.action labels, and refusal github.denier plus github.denier.delivery; document outcome, busy.cause and queue.depth as alarm-able.
11. Keep GithubEvent/GithubThread/thread() public for s5 consumers (thread() becomes the event method), GitHub Trigger::Message, githubEvents and wake_reason spellings, and context.request.body shape; keep decision logic testable in policy/broker/protocol.

## Must-holds and review-r4 corrections

| Requirement | Landing and proof |
| --- | --- |
| 1. Out-of-bound discussion category is absent | Optional authorSet.category in core/schema; synthetic discussion and discussion_comment parser tests use CJK slugs and assert acceptance with omission. |
| 2. Out-of-bound repository_dispatch action is absent | Optional action; parser tests use spaces and over-64-character event types and assert acceptance without action. |
| 3. Issue-template label warning | docs/security-model.md and the owner-item wording below explicitly warn against permits on template-applicable labels. |
| P3: action uses GithubWord | The validated constructor accepts [A-Za-z0-9_.:-]{1,64}; parser tests include OWNER, q-a and deploy:migrations and preserve their case. |
| P3: case is significant | docs/security-model.md says facts keep GitHub's case and Cedar == is case-sensitive; a forbid on "none" does not match NONE. |
| P3: six classifications | The owner-item wording below and security explanation say six, matching the six classification bullets. |

Owner-item wording carried by this PR: six classifications distinguish repository-set facts, which may permit, from author-set facts, which can only deny. Policies must not permit on a label an issue template can apply: an outsider choosing a template can receive its labels and assignees. Facts retain GitHub's case; Cedar equality is case-sensitive. Prose never becomes authority.

## §3.7 tests, written before implementation after approval

Use neighboring helpers: policy world/request fixtures, gateway signed() and existing parser bodies, broker configured_broker()/constraint_set()/request(), protocol scope(), and dekopon-test-support CaptureLayer.

| Test | Location and concrete failure it detects |
| --- | --- |
| 1. every_family_sample_validates_strictly_against_the_schema | policy/src/tests.rs: serialize every header family (including common-only families) and exercise the production context assembly path for capability, prompt and secret actions, each with the full event and with authorSet omitted. Strictly validate the complete routing/action/github context and load guarded policies reading family facts; assert that valid requests allow. Include the subsequent HTTP request merge and verify context.request.body retains its shape. Missing/mistyped schema attributes or converting github before required routing/action attributes are assembled fail this test. |
| 2. a_permit_on_an_author_set_fact_never_allows | Broker policy-decision tests exercise evaluate, with policy fixtures in the policy crate where useful: sole authorSet.headRef permit denies; matching forbid with broad permit denies; mismatching forbid allows. Include forbid-unless and evaluation-error cases so omitting the stripped pass fails. |
| 3. a_coalesced_turn_is_allowed_only_if_every_event_is | broker/tests/integration/policy_decisions.rs: issue_comment-only policy refuses [issue_comment, pull_request.synchronize] and permits two issue_comments. Evaluating only the lead fails. |
| 4. github_facts_must_match_the_attested_scope | Broker scope tests: foreign fullName, wrong thread, Slack kind, wrong trigger and first-delivery mismatch each assert attestation-denied; a matching claim succeeds. Omitting a coordinate check fails. |
| 5. an_unknown_field_is_ignored_and_only_a_required_fact_out_of_bound_is_malformed | Gateway parser tests: unknown fields pass; 41-hex required head SHA is malformed; 51-character label drops; 50-emoji label survives; word examples parse; spaced deployment environment is absent. Include both must-hold omission cases and synthetic bodies for the other families. |
| 6. broker_decision_names_the_waking_events | New broker/tests/github_authority_records.rs, following space_authority_records.rs with one global CaptureLayer: assert structured, typed, aligned arrays for two events and exact refusal phase/delivery; no log-text substring assertions. |
| 7. the_redelivery_bodies_parse_to_their_facts | Gateway GitHub tests read all 13 pinned files, sign with a test secret using signed(), and assert family, thread and at least two facts per body as parsed Values. Exercise parsing before eligibility filtering so no action is skipped. |
| 8. a_frame_with_an_unknown_key_is_refused | broker-protocol/src/tests.rs: every Target variant, including None {}, round-trips unchanged; extra top-level and leaf keys fail. Add the byte-identical absent-field non-GitHub regression. Flatten/strictness drift fails. |

Capture the red baseline for tests 1–8 before production changes; missing new types may initially fail compilation. Then rerun each behavioral assertion on the implementation and verify it fails when its enforcing change is removed.

## What the real bodies show

Copy the 13 JSON files unchanged into crates/dekopon-gatewayd/tests/fixtures/typed-events-deliveries/, preserving names; never push to the fixture branch. A bounded credential-shape scan found no GitHub-token, AWS access-key, private-key, Slack-token or Bearer-token shape.

The fixtures cover ten pull_request actions and three workflow_run actions only. All common and required family facts are present. pull_request.merged_by is null in nine PR bodies, so pullRequest.mergedBy is omitted there; workflow_run.conclusion is null for requested/in_progress, so authorSet.conclusion is omitted there. Both are already optional under §3.3; no additional required fact needs promotion to optional. Empty pull_requests in requested/completed runs stay empty and yield repository; in_progress has one same-repository PR and yields 780. No fact is invented. Other families have synthetic parser coverage, stated explicitly in the eventual PR body.

## Validation, ship order and review

For this note: run git diff --check and both documentation gates from ci.yml. Rust gates are deferred because the coordinator requires this pre-code stop.

After approval, run all brief gates: fmt; workspace all-target/all-feature locked clippy; locked release check for brokerd/gatewayd; machete; both daemon dependency gates and their Python tests; SDK wasm32 check; CI no-exporter check; warnings-denied workspace rustdoc; external-provider fixture fetch then workspace lib/bins/tests and doctests; cargo deny; workspace packaging when READMEs change; documentation duplicate/audit gates; affected provider example workspaces. Report every unrun gate and both documented root-only permission-test gaps if observed. Confirm production authorize calls remain solely in evaluate, digest stability and non-GitHub wire bytes.

PR2's broker and gateway ship and roll back together in one chart pin because ChatScopeClaim denies unknown fields; dekopon-console needs github: None at its next re-pin. No deployment is authorized here. After the first passing build, push and open the single PR against main titled "feat(policy): type every GitHub event family as Cedar context". Continue append-only pushes at least hourly. Coordinator-launched Astra review gates it; Claude review accompanies the wire/Cedar/model-text change.

The user-visible implementation commit carries exactly this physical line:
Changelog: Added: Cedar policies read context.github, the typed facts of every GitHub webhook family; author-set facts can only deny.

## Choices I made

- Extend core's existing github.rs and use typed leaf records plus a private bounded collection; delete gateway GithubEntry and its duplicate batch rather than retain a conversion layer.
- Keep payload extraction in gateway's existing GitHub transport; derive routing and notes from the resulting event.
- Store immutable redelivery fixtures under the gateway test tree because its parser and signed-request tests consume them.
- Use the first denying delivery in arrival order and its full/stripped phase for refusal telemetry while preserving aggregate policy errors/IDs.
- Keep mergedBy and conclusion optional as the real fixtures require; no additional §3.3 required fact becomes optional.
- Put the semantic author-set safety test at the broker's deciding function, where the frozen enforcement lives, reusing existing policy helpers.
- Carry the owner-item warning and six-classification wording in this note and eventual PR body, with the policy warning also in docs/security-model.md; the private campaign text is unavailable.
