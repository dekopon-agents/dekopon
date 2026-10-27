# Fauxlack feature approach checkpoint

Steering first line: `# Steering for GPT executors`

Steering: xrl/agents `cc070358504191cc739c6aaecf124e91da8682aa`, read with `git show` before other work.
Immutable base: `c1a2a19a243649016b813142dc92cd072cdabaf4`.
Branch: `codex/fauxlack-feature-1` in dekopon-agents/dekopon.
This is the requested pre-implementation checkpoint. No implementation or PR yet. Stop after pushing this note for the coordinator's Astra review; resume only on its reply. Delete NOTE.md in a later ordinary commit before opening the PR, leaving it in branch history and out of the final PR diff. Never amend or rebase pushed commits.

## Conflicts and readings I will follow

1. The appendix describes missing subject, scope, delivery and broker-space vocabulary. PR #785 (`059a5eba`) already supplies it at this base: core subject.rs:35, broker-protocol lib.rs:269, :468, conversation.rs:107, broker lib.rs:2836 and gateway session.rs:3601. Reading: consume those definitions unchanged; add only the explicitly deferred `ExternalSubject::fauxlack` constructor alongside its manifest/transport consumers.
2. The appendix says other participants queue during a running turn. PR #784 changed mapped-channel admission (session.rs:670-738; session/participants.rs:110-218). Reading: retain current s2b behavior. This PR's headline remains sequential, waiting for each completed answer. It makes no claim about running-turn participant legs or steering.
3. The appendix says ordinary chat claims carry `delivery: None`. session.rs:3524 initializes that field but :3528-3532 now fills it for native messages, including fauxlack through :3601. Reading: use the current claim builder and existing delivery identity; no frame, delivery or persistence alteration.
4. The config reference numbers moved: TransportConfig is config.rs:372; resolve is :1977; listener/indicator/normalization are :2031/:2054/:2072; resolved indicators are :2792. The shared boot/check candidate body is lib.rs:525, and reload also calls it (gateway.rs:1014). Reading: pass an optional service through existing validation/loading/preparation and reload, with cfg parameters where its type is needed. Do not create a second validator or move refusal into build_transport.
5. The transport loop skips duplicate names at config.rs:2026-2029. Reading: aggregate fauxlack service/attachment problems before that skip, including duplicate attachments, so duplicate-name and missing-service errors survive together.
6. The appendix headline sketch passes `field(decision, "decision.id")` directly into Audit::find, but gateway.rs integration helper `field` at tests/gateway.rs:519 strips surrounding quotes whereas `has` at :509 compares rendered fields. Reading: re-quote the extracted string for the execution join; check full principal, subject and conversation coordinates, not only thread id. The required eight model calls and call-id tool matching stay exact.
7. CONTRIBUTING.md:73 requires a non-test consumer for every public item, while the owner explicitly requires exported test handles and observation methods, and steering ordinarily forbids new public API to bypass internal limits. Reading: expose only the requested feature API; the explicit owner requirement controls those test-only entry points. Keep all world/driver implementation details private or pub(crate), with production-code consumers for constructor, transport, validation and run_fauxlack. No speculative public API.
8. docs/gatewayd.md:989-991 currently says the published gateway has no fauxlack config or service. Reading: update it to say the config variant is parsed in every build, boot/check refuse it without the in-process service, and world/transport/run_fauxlack exist only under the feature. Preserve the absence of a release service.
9. The appendix's `config.rs:1168` mixed-principal statement refers to brokerd config (now :1174), not gateway config (now :1217 is fragment handling). Reading: leave broker identity validation unchanged; manifests contain chat facts only.
10. The appendix gateway fixture sets only `indicators: { progress: message }`, but existing indicator mode defaults off (docs/gatewayd.md:95-96). Reading: set the existing `mode: native` in the progress fixture to exercise post/finalize, without changing defaults or adding a key.
11. The scripted OpenAI model returns text/tool calls, not outbound generated images. Reading: the files integration scenario attaches notes plus a manifest PNG, uses the existing real-broker `httpprobe asset-send` command to produce the reply's GeneratedImage, and checks its bytes/name/MIME. A direct driver test also supplies GeneratedImage::from_png. No synthetic model-image output or new provider component is introduced.
12. The checkpoint explicitly precedes building and PR creation, whereas normal steering asks for early builds/drafts and a full final PR report. Reading: push only this approach note now and stop, as the coordinator explicitly restated; implementation gates, PR and final delivery report follow review.

No contract or authority stop was found. No new config keys beyond the approved fauxlack variant/schema, normal dependencies, chart values/mounts, WIT, frames, persistence rules, grants or live changes are planned. The newer origin/main and release preparation do not replace the stamped base.

## Shape and implementation order

1. World and manifest: add dependency-free `fauxlack = []` to gatewayd. Gate the exported module and transport module at their declarations. Add the core constructor using existing strict canonical parsing/validation, without duplicating grammar or accepting case folding for manifest IDs. Validate person/space kinds from core and canonical conversations from protocol (including empty-member containers using a valid synthetic sender only for the conversation check). Preserve typed authority errors in aggregate manifest problems. Serde strict authored structs cover root, server, user, channel, DM, file and image. Report semantic conflicts together after decoding; YAML syntax/unknown-field failures remain typed decode errors. Users/channels/DMs share an ID namespace, files have their own name namespace, and separately named DMs may share members.
2. Shared config validation: add unconditional `TransportConfig::Fauxlack { name, server, bot, indicators }` (Rust field can follow the existing `liveness` spelling with serde rename). Add typed missing-service, unknown-server, unknown-bot, non-bot and duplicate-attachment problems consumed by shared resolve. `run`, `check`, telemetry/config entry points and the executable always use no service; only run_fauxlack supplies it. Validation must precede transport construction and aggregate with existing problems in files and directory fragments.
3. Transport/driver: attach one bounded 64-event receiver per configured server/bot. Project only manifest identities; deliver only to member bots, including self, and drop all bot authors in receiver order. Token-derived mentions control shared-conversation addressing; DMs use None. Channel roots and replies share channel:root keys; DMs have no native thread. Reuse bound_inbound, receive_span and record_conversation. Reply and message progress are the only driver surfaces. Existing defaults supply None for all others. Asset fetch reads immutable world file bytes via existing bounded fetch interfaces; generated reply images become world files. Keep foreign-target refusals in every real adapter.
4. Runtime: run and run_fauxlack call one serving body with an optional feature-only service. Carry the handle through validation/preparation and resident reload state; do not rebuild or reattach resident transports on config reload. Shared check and release builds always refuse fauxlack without a service.
5. Tests and docs: extend the existing integration fixture setup, not a parallel harness. boot_fauxlack selects feature fixtures while reusing boot_with's real broker/model/shutdown lifecycle, private config writer, provider helper, Audit and response scripting. Add a call-id-based Fixture::tool_result. Add the feature module at the end of tests/gateway.rs. Test fixtures are disposable and never read live state.

The world is owned by the Fauxlack Arc, with short parking_lot bookkeeping locks and one watch change counter. No task per post. Posting must preserve order even across queue backpressure: a per-server admission permit serializes post delivery while bounded receiver reservations wait without holding a world lock. Queue senders belong to attachments; gateway readers retain the existing JoinSet shutdown owner. World rows live only for this finite disposable service; no production store or unbounded background work is added. The implementation will explicitly document owner/bound at any lint expectation it needs.

Completed answers have a completion sequence separate from minted message order: reply publication or progress finalization appends once; progress edits do not append; deleting progress removes it. A Thread cursor starts at handle creation, advances once per returned completion, and watch waits cannot miss mutations. Transcripts sort by numeric minted ID and exclude unfinished/deleted progress. Observation handles must exist before delivery can finish; tests will use existing held-model synchronization or create observers before posting, rather than relying on scheduling speed. No sleep-based negative proof.

## Files I expect to change

- `crates/dekopon-core/src/subject.rs`: constructor and its direct invariant test.
- `crates/dekopon-gatewayd/Cargo.toml`: empty feature only.
- `crates/dekopon-gatewayd/src/fauxlack/{mod,manifest,world,tests}.rs`: feature API, immutable facts, world transitions and focused tests; split only where ownership warrants it.
- `crates/dekopon-gatewayd/src/transport/fauxlack.rs`: transport, reply/progress driver, asset fetcher.
- `crates/dekopon-gatewayd/src/{lib,config,gateway}.rs`: service plumbing, aggregate validation, resident reload owner.
- `crates/dekopon-gatewayd/src/transport.rs`, `src/transport/local.rs`, `src/asset.rs`, `src/asset/storage.rs`: unconditional private target/source variants and exhaustive consumers. Other real adapters only if the compiler requires explicit refusal arms; existing let-pattern refusals stay intact.
- `crates/dekopon-gatewayd/src/tests.rs` and/or `src/tests/fauxlack.rs`: shared check/run boot refusal, aggregation, indicator resolution, resident reload coverage and non-feature serde coverage. Extend nearby existing tables rather than duplicate suites.
- `crates/dekopon-gatewayd/tests/gateway.rs`, `tests/gateway/fauxlack.rs`, `tests/gateway/team.yaml` and a two-server manifest fixture: reuse the real harness and add required scenarios.
- `docs/gatewayd.md`, `docs/development.md`, `crates/dekopon-gatewayd/README.md`: test-only config/service/asset qualifications and harness command.
- NOTE.md removal before the PR. No CHANGELOG.md, Cargo.lock, CI, chart, WIT, provider fixture source or persistence implementation change is intended.

## Tests to write first and their failure witnesses

- Aggregate manifest test containing every semantic problem kind, matching typed variants and preserved core/protocol errors; fails if validation short-circuits or hand-rolls permissive IDs. Strict nested-field decode cases; fails if deny_unknown_fields is absent. Include bad user/space kinds, mismatched canonical coordinates, duplicate server/shared IDs/file names, unknown/repeated members, undersized DMs, invalid file alternatives/base64, and accepted independently named same-members DMs.
- World completion observation: posted replies and finalized progress visible once in completion order; edits/unfinished/deleted progress invisible; independent cursors and wakeups. Fails if progress is mistaken for an answer or waits lose changes. A bounded-queue/order test exercises backpressure without adding a service dispatcher task.
- Boot refusal in feature and ordinary builds: strict fauxlack config parses, run/check reject no service, and duplicate transport is reported alongside it. With service, unknown server and a non-bot attachment in separate transports are reported together; cover unknown bot and duplicate attachment. Fails if validation is build-only or skips later conflicts.
- Headline `sequential_turns_in_a_mapped_thread_each_run_as_their_asker_intersected_with_the_space`: four completed turns and exactly eight model requests; a1/b1/a2/a3 each resolved by tool_call_id (duplicates with identical replay content allowed; zero or conflicting distinct results panic). SHIP/WAIT succeed in mapped team scope, reverse is NotGranted there and succeeds in Alice's DM. Join each successful execution to its decision by rendered decision.id and assert principal, subject, fauxlack transport, acme container, cteam/root or dalice/no space. Assert transcript author sequence. Fails for wrong asker, missing space downscope, call/result mismatch or extra model work.
- Unauthorized Carol: observe her completed named refusal, observe Alice's subsequent control answer, then exact model count (one control request with a final-only script). Fails if the denied post calls the model or the receiver is dead.
- Bot suppression: helper mention followed by Alice control answer, empty suppressed-thread answers and exact count of one. Membership: crandom post followed by cteam control answer, likewise empty and exactly one. Fails for bot ingress, delivery outside membership or hidden extra requests.
- Two-server isolation: same IDs in acme/globex, two transports, separate mapped people/spaces; observe each answer only on its server and the other transcript empty before its own input. Exactly two final-only model requests. Audit subject, space and container must agree per server. Fails for cross-server keying or mapping leakage.
- Files both ways: notes.txt attachment reference appears in raw recorded request, scripted generated image bytes/name/MIME are present on the completed bot answer; exactly two model requests (one existing httpprobe asset-send tool call and one final reply). The PNG is an additional manifest attachment, served by the real fauxlack fetcher and sent through the existing broker-authorized asset path; the integration test asserts its source bytes, not an image-generation claim. Reuse existing attachment/image fixtures and commands; do not invent model image authority. Fails if ingress attachment projection/fetch or outbound file delivery is missing.
- Reload retains the service/attachment while applying a permitted existing route change; an observed next answer proves it remains live. No new reload policy, knob or capability.

## Acceptance and gates planned after review

| Acceptance | Proof |
|---|---|
| Headline, eight requests, call matching, intended authority and joined Succeeded records | Exact headline test above, run by feature integration command |
| All section 6 scenarios with negative controls/exact counts | Feature integration tests above |
| No-feature build excludes world/transport code | cfg module declarations inspected; `cargo build -p dekopon-gatewayd --locked` and no-feature release check |

Run `ci/fetch-external-provider-components.sh examples/providers` before Rust validation. Use the pinned toolchain and --locked throughout. Check disk before expensive builds and follow docs/development.md artifact ownership/lifecycle. No builds at this approach checkpoint.

- `git diff --check`; `cargo fmt --all --check`.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`.
- `cargo check --release --locked -p dekopon-brokerd -p dekopon-gatewayd -p dekopon-console` with no feature flags.
- Both normal-dependency boundary steps copied unchanged from .github/workflows/ci.yml:96-120, plus `python3 .github/scripts/test_daemon_dependency_gates.py`; compare the gateway normal tree to base to confirm no added normal dependency.
- `cargo test --workspace --all-features --locked --no-run`, then `--lib --bins --tests` with CI's DEKOPON_TEST_BROKERD/PROBE_WASM/ASSET_WASM fixture paths, then `--doc`.
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`.
- `cargo deny --all-features check`; `cargo machete`.
- `python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md`; CI's existing audit-event documentation check (no new event names planned).
- `cargo test -p dekopon-gatewayd --features fauxlack --locked --test gateway`.
- `cargo build -p dekopon-gatewayd --locked` and targeted no-feature refusal/serde tests.
- Additional metadata gate required by CONTRIBUTING.md: `cargo package --workspace --locked` from the clean final tree. No fixture/WIT regeneration or provider source gates unless the actual scope changes (a contract stop would be reported first).
- Observe remote CI on final head, coordinator's fresh Astra review over immutable base/head, then fresh Opus xhigh review coordinated as requested. Do not self-certify reviews or invoke paid/live services.

At final delivery enumerate all gates actually run and every gate unavailable/failed with its exact reason; report actual head, PR and diff size. An unrelated CI failure is evidenced and rerun without an unrelated fix.

## Readers and ship order

PR1 vocabulary is merged before this feature producer. Older readers reject new subjects/enum variants and cannot downgrade a disposable store containing them; no shim or migration. Preserve existing values. No feature producer enters release binaries.

Readers requiring deployed-version verification remain **pending verification**: brokerd (config subjects, frames, workflows), gatewayd (config, wake, journal), console (--subject/profiles), tui, and providers pinning core/protocol. This task authorizes no live reads or deploys, so repository version 0.60.0 at this base is not presented as deployed evidence. The release-prep branch does not change this branch. Slice B's later deployed Slack audit read remains coordinator-owned, not a claim made by this fake transport.

## Citation reread ledger

Every cited location was re-read at the stamped base, following moved code rather than trusting old line numbers. The mapping below records line drift and semantic drift; unlisted anchors in a listed file retain their old line number. Lines are old appendix -> stamped-base lines. The scope/readings above resolve the semantic differences.

- `crates/dekopon-core/src/subject.rs`: 45→46, 58→60 (changed region), 62→67, 105→115, 188→199, 227→240, 293→306, 445→458, 467→482, 491→506, 511→528, 526→545, 562→587.
- `crates/dekopon-broker-protocol/src/lib.rs`: 280→282, 451→461, 575→597 (changed region), 1481→1502.
- `crates/dekopon-broker-protocol/src/conversation.rs`: 107→120, 176→189, 187→200, 220→239, 395→414, 423→442.
- `crates/dekopon-broker-protocol/src/tests.rs`: 922→924, 1265→1377, 1283→1400, 1368→1513, 1423→1588, 1625→1851.
- `crates/dekopon-broker/src/lib.rs`: 2836→2837, 2841→2842, 4546→4547, 4599→4602, 4837→4841, 4846→4850, 4855→4860, 4906→4911, 4926→4931, 4942→4947, 4953→4959.
- `crates/dekopon-broker/tests/space_authority_records.rs`: 146→172, 212→260, 215→263, 237→298.
- `crates/dekopon-brokerd/src/config.rs`: 192→193, 1168→1169, 1173→1174.
- `crates/dekopon-brokerd/src/workflows.rs`: 468→469.
- `crates/dekopon-brokerd/src/server.rs`: cited anchors unchanged.
- `crates/dekopon-agent/src/lib.rs`: 772→808, 822→858.
- `crates/dekopon-gatewayd/src/lib.rs`: 576→584, 580→588, 717→726, 964→974, 1022→1032.
- `crates/dekopon-gatewayd/src/config.rs`: 2386→2549, 2394→2557, 1868→2031, 1891→2054, 1909→2072, 2627→2792, 362→365, 369→372, 462→465, 474→477, 486→489.
- `crates/dekopon-gatewayd/src/gateway.rs`: 683→687.
- `crates/dekopon-gatewayd/src/session.rs`: 672→700, 694→723 (changed region), 816→855, 2824→2974, 3066→3216, 3372→3524, 3393→3553, 3434→3593.
- `crates/dekopon-gatewayd/src/transport.rs`: cited anchors unchanged.
- `crates/dekopon-gatewayd/src/asset.rs`: cited anchors unchanged.
- `crates/dekopon-gatewayd/src/asset/storage.rs`: cited anchors unchanged.
- `crates/dekopon-gatewayd/src/transport/slack.rs`: cited anchors unchanged.
- `crates/dekopon-gatewayd/src/transport/discord.rs`: cited anchors unchanged.
- `crates/dekopon-gatewayd/src/transport/local.rs`: cited anchors unchanged.
- `crates/dekopon-gatewayd/src/collection.rs`: 543→544, 596→597.
- `crates/dekopon-gatewayd/src/wake.rs`: 294→295.
- `crates/dekopon-gatewayd/src/wake/store.rs`: cited anchors unchanged.
- `crates/dekopon-gatewayd/src/memory/journal.rs`: cited anchors unchanged.
- `crates/dekopon-gatewayd/src/progress/policy.rs`: cited anchors unchanged.
- `crates/dekopon-gatewayd/src/progress/tests.rs`: cited anchors unchanged.
- `crates/dekopon-gatewayd/tests/gateway.rs`: cited anchors unchanged.
- `crates/dekopon-tui/src/session/tests.rs`: cited anchors unchanged.
- `crates/dekopon-console/src/main.rs`: cited anchors unchanged.
- `.github/workflows/ci.yml`: 458→466.
- `.github/workflows/release.yml`: cited anchors unchanged.
- `charts/dekopon/templates/_helpers.tpl`: 304→306.
- `docs/gatewayd.md`: 540→571, 938→969, 1418→1502, 1635→1719 (changed region).
- `docs/development.md`: cited anchors unchanged.
- `docs/observability.md`: 498→502 (changed region).
- `docs/security-model.md`: cited anchors unchanged.
- `crates/dekopon-brokerd/README.md`: 649→649 (changed region).
- `crates/dekopon-broker-protocol/README.md`: cited anchors unchanged.
- `crates/dekopon-gatewayd/README.md`: cited anchors unchanged.
- `AGENTS.md`: cited anchors unchanged.
- `CONTRIBUTING.md`: cited anchors unchanged.

Also re-read the publication list, Dockerfile header, test-support manifest, AGENTS.md, CONTRIBUTING.md, the gateway/development manuals, constitution/non-goals, security authority sections, and appendix sections 1–8 including the full headline sketch. Gateway normal dependencies already contain serde_yaml/base64/tokio/parking_lot; the feature adds no dependency. No uncited platform behavior is claimed.

## Choices I made

- Follow the coordinator's immutable stamped base despite newer origin/main/release refs.
- Treat already merged vocabulary and s2b behavior as current; preserve the headline's sequential scope over the appendix's outdated running-turn explanation.
- Correct the audit sketch's decision-id rendering using the existing helper contract, without changing Audit semantics.
- Thread the optional service through the existing config/candidate/reload path rather than add a second validation/boot mechanism.
- Validate service attachments before duplicate-name short-circuiting to keep the required aggregate report.
- Use core's existing strict canonical path for the new constructor and protocol's canonical conversation validator for manifest authority errors.
- Keep the owner-requested feature-only test API public despite the general non-test-consumer rule; implementation details stay crate-private.
- Share boot_with's existing fixture lifecycle; select fauxlack fixtures behind cfg rather than duplicate broker/model setup.
- Use one world watch counter and separate completion sequencing; create observers before delivery can finish to honor cursor semantics.
- Serialize backpressured posts with a per-server admission permit and bounded attachment queues; never await while holding the world bookkeeping lock.
- Remove NOTE.md with a later commit so it remains reviewable in history but absent from the final PR diff.
- Enable existing native indicator mode in progress fixtures instead of treating progress: message alone as enabling it.
- Exercise the outbound GeneratedImage representation through existing real-broker asset-send plus a direct generated-image driver test, without claiming the model generates pixels.
