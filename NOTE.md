# Fauxlack vocabulary approach checkpoint

Steering: `# Steering for GPT executors`, read from xrl/agents commit
`cc070358504191cc739c6aaecf124e91da8682aa` after fetching that missing commit.
Base: `999c6c05a785c7beca52903883aece2b87f8f0a6` (v0.59.0).
Branch: `codex/fauxlack-vocabulary-1`.
This is the approach-only checkpoint. No implementation has started. The coordinator
launches Astra review of this immutable note commit; implementation waits for its reply.

## Conflicts and readings I will follow

- Appendix A2 describes unconditional `ReplyTarget`, `LivenessTarget`, and
  `AssetSourceRef` variants. Contract 1 and the PR1 checklist authorize only
  `SubjectService::Fauxlack`, `ChatTransportKind::Fauxlack`, and
  `DeliveryIdentity::Fauxlack { channel, message }`. Follow PR1: leave those private
  gateway variants, transport configuration, manifest, and constructor to PR2.
- The general exhaustive-match rule conflicts with the explicit instruction not to
  add arms to review-only `api_channel`, `has_container`, collection and wake paths.
  Their defaults already give fauxlack the intended behavior. Follow that explicit
  instruction; review and test existing behavior without adding those arms.
  Convert the touched core `requires_tenant` match and protocol delivery matching
  to exhaustive dispatch so the hidden cases named in the brief cannot recur.
- `DeliveryIdentity::is_canonical_for` currently ends in `_ => false` (lib.rs:575).
  Follow the exhaustive-match contract by explicitly handling mismatched delivery
  and transport kinds rather than retaining or adding a catch-all.
- AGENTS.md requires deployed-reader artifact verification before wire changes;
  the owner brief explicitly requires deployed versions marked `pending verification`
  and forbids touching live systems. Follow the brief: list brokerd, gatewayd,
  dekopon-console, dekopon-tui and version-pinned providers as pending verification,
  verify old-reader refusal from source/tests, and claim no deployed verification.
- GPT.md generally prohibits new public API, but defers to the brief. Add only the
  three authorized public variants; do not add `ExternalSubject::fauxlack` in PR1.
- GPT.md asks for a full PR body and fenced diff in the report. The brief limits
  the report to 300 words. Follow the brief; put detailed evidence in the PR body.
- The brief requires a pushed NOTE.md outside the eventual PR diff. Commit it now,
  then remove it with a new commit during implementation; never amend a pushed commit.
- Repository guidance says branch from current main/rebase; the brief pins this base
  and prohibits rebasing a pushed branch. Use the exact pinned base and merge main
  only if a later conflict requires it. Fetching existing tags is steering-mandated;
  create, move and push no tag.

## Base review and drift

The requested base is available and checked out. Reviewed AGENTS.md, constitution,
non-goals, change maps, CONTRIBUTING.md, validation commands and PR template.
The Appendix B source anchors resolve to the described behavior at this exact base;
no material line/behavior drift found in that checklist. Existing wildcard/matches!
patterns are intentional review points above, not evidence that fauxlack is supported.

Checklist disposition before implementation:

| Area | Planned work or reviewed disposition |
| --- | --- |
| core subject.rs:34,45,58,62,105,188,293 | Add service/spelling/parser; required tenant; Slack segment grammar; c-prefix space classification; nonnumeric-service branch. |
| protocol lib.rs:266-280,451,575 | Add kind/Display and bounded tagged channel/message delivery; canonical scope check and exhaustive mismatch refusals. |
| protocol conversation.rs:100,187,395-423 | Subject service and exact tenant/container equality; lowercase id; DM kinds without thread; Channel/Thread with m-number thread; all four produced kinds; token selector grammar. |
| conversation.rs:79,220 | Already correct (reviewed): API channel is id except Discord thread; fauxlack has container. No new arm. |
| broker lib.rs:2836,4546,4599,4942,4953 | Add space service, unchanged lowercase HTTP slots, console pass-through, delivery audit spelling and message id. |
| brokerd config.rs:192,1173 | Already correct (reviewed): typed ExternalSubject and kind-based mixed-principal validation consume core. No config keys added. |
| brokerd workflows.rs:29,54,468 | Typed inputs already consume core/protocol; extend synthetic conversations for the four fauxlack kinds. |
| gatewayd session.rs:3434 | Construct fauxlack delivery from conversation id and native message id. |
| gatewayd collection.rs:115; wake.rs:294 | Already correct (reviewed): no collection, default horizon. No new arm. |
| tui session/tests.rs:37 | Extend explicit non-Local alternatives for exhaustive fixture assertion. |
| test tables listed in Appendix B | Extend existing core/protocol/collection tables and broker audit/refusal fixtures; no new harness. |
| docs/security-model.md:53 | Describe two accepted test-only subject forms and unchanged mapping/grant authority. |
| brokerd README.md:649 | Correct the person/space classification sentence. |
| protocol README.md:110 | Document fauxlack channel/message delivery form. |
| docs/gatewayd.md:938,1418,1635; observability.md:498 | Document accepted subject/kind vocabulary without suggesting a runnable fauxlack transport. |

Older readers reviewed: core rejects UnknownService; protocol enums and frames decode
strictly; gateway TransportConfig remains strict and receives no new kind; wake and
journal decoding fail as corruption. Existing persisted values keep their representation.
No migration or shim is needed. No deployed producer is introduced.

## Files expected to change

- crates/dekopon-core/src/subject.rs
- crates/dekopon-broker-protocol/src/{lib.rs,conversation.rs,tests.rs}
- crates/dekopon-broker/src/lib.rs
- crates/dekopon-broker/tests/space_authority_records.rs
- crates/dekopon-brokerd/src/workflows.rs
- crates/dekopon-gatewayd/src/{session.rs,collection.rs} (collection tests only)
- crates/dekopon-tui/src/session/tests.rs
- docs/{security-model.md,gatewayd.md,observability.md}
- crates/dekopon-{brokerd,broker-protocol}/README.md

Compiler-discovered exhaustive consumers may require equivalent arms in existing
files. Any extra wire/config/WIT change stops; no dependency or package-version
change is planned. NOTE.md is removed before opening the PR.

## Shape and authority

Use the current parser/segment validation and PrincipalKind dispatch. A required
fauxlack tenant uses Slack's lowercase alphanumeric grammar; only c-prefixed ids
are spaces. Do not add a separate subject parser or public constructor.
Reuse existing lowercase-token and canonical unsigned-decimal validation in protocol
canonical methods: strip `m` and validate its suffix as a canonical positive u64
(no zero or leading zero), shared privately if needed by thread and delivery checks.
A fauxlack conversation must name the sender's tenant as its container. Channel
and Thread require a thread; DirectMessage and GroupDirectMessage forbid one.
Delivery channel equals conversation.id and message uses the same m-number rule.

Extend broker space derivation alongside Slack/Discord. Sender mapping, namespace
grant, agent.prompt policy and asker/space policy remain the existing authorities.
No principal supplied by the gateway, new grant, bypass or broker refusal is added.
Transport identity still comes from authenticated ingress. Daemons stay separate.

## Tests to write first

1. Extend core tables at :445,467,491,511,526,562: cteam Space and ualice Person;
   parse/Display/serde round trips; missing tenant, uppercase/bad segments and extra
   segments refused; namespace disjointness from Slack. No raw fauxlack constructor.
2. Extend protocol canonical table (:922): all four conversation kinds accepted;
   missing/different container, wrong service, uppercase id, missing/bad thread for
   Channel/Thread, and threaded DM kinds refused. Extend API-channel assertion
   (:1265), selector validation/matching (:1283,:1368), delivery checks (:1423),
   and bounded decode checks (:1625). Assert literal `fauxlack` serde/Display round
   trips and the exact tagged delivery JSON. Removing the relevant variant arm,
   tenant equality, thread rule or bound must fail these cases.
3. Extend collection tables (:543,:596): fauxlack bypasses collection and preserves
   native attachment/overflow behavior. A fauxlack batching branch must fail them.
4. Extend existing broker fixture with fauxlack.team.uasker -> asker and
   fauxlack.team.cchannel -> space. Add the fauxlack case to the existing audit
   scenario (one global CaptureLayer initialization), asserting principal, space,
   both verdicts, denied_by, transport kind, all coordinates and delivery kind/id.
   Removing fauxlack space derivation must fail the space-denial assertions.
   Extend cross-transport delivery and attested-space sender refusals, including
   mapped and unmapped fauxlack spaces, using current helpers. Accepting a wrong
   transport or space sender must fail these cases.

## Gates after review and implementation

First fetch fixtures with `ci/fetch-external-provider-components.sh examples/providers`.
Check artifact ownership/free space before builds; retain no unowned build cleanup.
Use the pinned toolchain and --locked for Cargo graph/build/test commands.

- git diff --check
- cargo fmt --all --check
- cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
- cargo check --release --locked -p dekopon-brokerd -p dekopon-gatewayd -p dekopon-console (no features)
- Both exact daemon normal-dependency tree checks from ci.yml:96-120
- python3 .github/scripts/test_daemon_dependency_gates.py
- cargo test --workspace --all-features --locked --no-run
- Workspace --lib --bins --tests with CI's DEKOPON_TEST_BROKERD/PROBE_WASM/ASSET_WASM paths
- cargo test --workspace --all-features --locked --doc
- RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
- cargo deny --all-features check
- cargo machete (CI version 0.9.2)
- python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md
- Documentation audit-event coverage gate from CI (no new event names intended)

Run focused affected-crate tests before broad gates. Report every unavailable or
failed gate with exact evidence. No Rust gates have run at this approach checkpoint.
Final ready PR requires green gates and exact-head CI; otherwise report the concrete
failure and use draft status. Coordinator launches Astra, then fresh Opus xhigh
review of final head; executor does not self-certify those reviews.

## Choices I made

- Treat Appendix A2 gateway variants as PR2 scope under the explicit three-addition contract.
- Preserve review-only default behavior rather than adding arms expressly forbidden by the checklist.
- Make touched tenant and delivery dispatch exhaustive while preserving all existing values.
- Interpret m-number identifiers using the existing canonical positive u64 decimal predicate.
- Extend the existing audit scenario instead of creating a second global subscriber initialization.
- Keep the note in branch history and remove it in a later commit so it is absent from the PR diff.
- Keep deployed-reader versions pending verification as authorized; make no live-system requests.
