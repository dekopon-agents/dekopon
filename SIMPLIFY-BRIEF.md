# Dekopon simplification — amended execution brief

**Canonical execution document.** The external draft's §9 corrections are applied below,
not retained as a competing addendum. A fresh agent reads this file and `SIMPLIFY.md`.
All decisions are settled. Do not ask the owner to select options, reopen decisions, or
retain code for idempotency, exactly-once, reconcilability, or crash durability.

**Core repository:** `github.com/dekopon-agents/dekopon`, checkout
`~/code/dekopon/dekopon`. Base: `origin/main` @ `542430e` (PR #186, configurable
conversation history scope). The integration worktree recipe is:

```sh
git -C ~/code/dekopon/dekopon fetch origin && git -C ~/code/dekopon/dekopon worktree add \
  ~/code/dekopon/.worktrees/simplify -b simplify/2026-09 542430e
```

**Console repository:** `github.com/dekopon-agents/dekopon-console`, checkout
`~/code/dekopon/dekopon-console`; its verified base is `origin/main` @ `ef0bf3f`.
Give it a separate integration worktree and branch. It is unpublished; its Dekopon
library dependencies are pinned to published `=0.11.1`. Keep those pins.

There were zero open core PRs at the start. #187/#189/#190 are closed, unmerged;
`feat/harness-session-runtime`, `refactor/harness-journal-only` and
`refactor/harness-journal-naming` are sources to read with `git show`, not bases to
check out, cherry-pick wholesale, merge or reopen. There is nothing to close.

Citations are navigation hints into `542430e`, not the evolving branch. Re-take them
with `grep -n`. The prior reviewer verified only the corrections incorporated here,
not all of §5–§8; missing validation is not success.

**Toolchain:** root MSRV is Rust 1.89.0; normal toolchain is stable. The six checked
provider build scripts need Rust 1.97.0, wasm-tools 1.236.1 and wkg 0.16.0 when their
inputs change. No planned unit changes `dekopon-core`, `dekopon-provider-http` or
`dekopon-provider-storage`. Published core crates are at 0.11.1 (the 0.12.0 publish
stopped partway). Nothing in this brief authorizes crates.io writes, yanks, tags,
releases, PR merges, or deployment to a live cluster.

## 1. Goal and non-goals

Make **credentials unleakable**, so the owner can grant other people access to their
agents safely. Does a subsystem serve credential containment, or earn its lines as
product surface? That is the test.

Idempotency, reconcilable state, crash durability and exactly-once semantics are
explicit non-goals. If a call fails, the model re-assesses and retries. Reject arguments
for retaining code on those grounds. Record disagreement in `SIMPLIFY.md` and proceed
as written; only §5's four triggers halt execution for an owner decision.

**D6 is the highest-value work.** Shipping the real process/credential boundary alone
is better than every deletion without it. Do not let deletion throughput crowd it out.

## 2. Baseline

At `542430e`: **74,903 production / 57,386 test / 132,289 total Rust lines**, 43% test
code; **26 crates**. Re-measure and report both the stated and observed numbers if they
differ. This script is the production-line authority:

```sh
# Run from the measured worktree (all Git operations still use git -C).
for f in $(find crates -name '*.rs' -not -path '*/tests/*' -not -name '*test*'); do
  m=$(awk '/^#\[cfg\(test\)\]|^mod tests|^pub mod tests/{print NR; exit}' "$f")
  [ -n "$m" ] && echo $((m-1)) || wc -l < "$f"
done | awk '{s+=$1} END {print s}'
```

Test lines are total Rust lines minus this production count. A naive first-cfg-only
count differs by about 3k because ten files have test-only helpers above production code.
Longest file: `crates/dekopond/src/tests.rs`, **9,879 lines**. Final crate target: **22**.

Informational production mass: dekopond ~11.1k (five transports ~5.9k), shell ~10.4k,
brokerd ~8.4k, storage-host 7,253; broker 5,942 + broker-host 3,482 + broker-protocol
2,742 + policy 1,423 = 13,589. No unit is gated on these per-crate estimates.

## 3. Constraints to verify cheaply at each owning seam

- `jq` wraps jaq, not a new implementation. `curl` is a capability-proposal parser,
  not an HTTP client. Keep the shell interpreter, parser, limits and all text builtins.
  No environment/filesystem/socket access is added; jaq's env filter stays unlinked.
- `dekopon-process` belongs to unprivileged orchestration, never brokerd. Moving an
  interpreter of untrusted model text into the credential-holding broker is forbidden.
- `dekopon-provider-host` is the import-free linker, used only by the direct runner.
  Delete it with the runner. `dekopon-broker-host` is the separate privileged HTTP/storage
  linker and stays. The SDK is the canonical provider WIT source; provider-host is a mirror.
- The harness rename is not taken: **keep `dekopon-agent`**. `journal.rs` exists only
  on the closed #190 branch and persists nothing; no unit acts on it. No worker checks
  out that branch or introduces `dekopon_harness::` paths. Main Slack code is
  `dekopond/src/transport/slack.rs`, not #187's `slack/progress.rs`.
- Real durability code: `storage-host/transaction.rs` (1,931 production / 2,725 total),
  `gc.rs` (432); `brokerd/checkpoint.rs` (451 production / 804 total), `brokerd/audit.rs`
  (160 production / 310 total), broker `FileAuditLog` (:2217) and `verify_audit_chain`
  (:2707). Checkpoint wraps the audit chain head, so D2b precedes D2c.
- `.github/scripts/verify-release-metadata.py` rejects an unconsumed library (:62),
  and a release list differing from publishable members (:132). Every crate deletion
  updates workspace members/dependencies, generated `Cargo.lock`, and
  `.github/release-crates.txt` **in that same commit**. D3c deletes run + provider-host
  together; D3d is folded in.
- Chart probes use `dekopon-run broker capabilities` (`_helpers.tpl:477/:481`, chart
  README :158–186, values :243–252). D3g replaces them before D3c.
- The provider-host WIT mirror is compared in `wit-package.yml:192–194` and included
  in path filters at :9/:36. Remove those retired-mirror references with D3c, retaining
  all canonical SDK and surviving guest/broker mirror validation.
- ChatGPT `login`, `status`, `logout`, `export_credentials` are library functions in
  `dekopon-model/src/chatgpt.rs:278/:313/:338/:403`; their only caller is the CLI's
  `auth.rs` (206 production lines). The owner's credential path must move to
  `dekopond auth chatgpt` **before the CLI disappears in D7b**.
- brokerd already maps `peer_cred()` UID to `BTreeMap<u32, MappedPeer>` in
  `server.rs:352`. The startup refusal at `lib.rs:128–133` prevents distinct UIDs.
  `socket.rs:21/:109` and broker-protocol `lib.rs:2009` enforce the current private
  parent/0600 socket. Gateway `broker.serverUid` exists (`config.rs:154`, default
  caller euid at :1068). D6 changes only the necessary socket/peer boundary, not identity
  mapping or credential/private-store permissions.

## 4. Settled decisions and unit contracts

One commit per **work unit** in §5, not per numbered decision. D4 and D5 are keeps.
Only named scope is authorized; adjacent discoveries go in the ledger, not the patch.

### D1 — Two port units from the closed stack

These are **behavior ports, not cherry-picks**. Candidate commits touch branch-only
harness/control/progress files and do not apply to main. Read source commits with
`git -C <repo> show <sha>` and implement at the main seams below. Write fresh main-tree
tests; do not import the source stack's tests or machinery.

**D1a — Slack 429, port of `b3bb68b`.** In `dekopond/src/transport/slack.rs:1081`, a
`chat.postMessage` HTTP 429 currently becomes reply-failed. Read Retry-After seconds
(cap 60, default 5 if missing/unparsable), wait and retry the identical post **once**.
A second 429 uses the existing failure path. Add one warn event
`gateway_reply_rate_limited`, with backoff seconds and no message text, and document
it in the gateway table of `docs/observability.md`. Add one loopback Slack-stand-in
TcpListener test in `dekopond/src/tests.rs` (existing pattern :1314): first response
429 / Retry-After: 1, second 200 / {"ok":true}; assert one delivery and no reply-failed.
No parking, quarantine, pacing framework, or new inline HTTP mock. Crate: dekopond.

**D1b — aggregate startup refusals, two related halves in one unit.**

1. Policy half (`c16462e`): `PolicyWorld::new`, policy `lib.rs:155–175`, collects every
   reserved-action and duplicate-capability conflict instead of returning the first.
   **Authorized public API change:** replace `PolicyBuildError::ReservedAction` and
   `::DuplicateCapability` with `::WorldConflicts { reserved: Vec<CapabilityId>,
   duplicates: Vec<CapabilityId> }`, never both empty. Rewrite the only old-variant
   matches in policy tests :523/:537; add a case with two reserved and two duplicate
   names and assert all four are named. The protocol's unrelated
   `InventoryError::DuplicateCapability` stays. Skip the source commit's control.rs
   policy-error downgrade fix: that machinery does not exist on main.
2. Transport half (`505587c`): collect all `connect()` failures at
   `dekopond/src/lib.rs:195–197` and render them together, as credential Startup already
   does. **Authorized public API change:** `DekopondError::TransportConnect` (:985)
   carries a Vec of transport-name/error pairs. Add one test with two failing transports.
   Do not import pacing/quarantine docs. Crates for D1b: policy and dekopond.

This reconciles §9's **two** port units and the owner's D1a/D1b effort assignment without
dropping either specified refusal fix. There is no D1c worker/commit slot.

**Dropped: Cedar freshness fence (`89cd02d`).** It repairs #187's harness disclosure gate,
which has no main counterpart. Main asks the broker afresh per message
(`dekopond/src/session.rs:763`, never cached as permission); broker dispatch reauthorizes
invocations. Do not build a new client-side fence.

### D2 — Drop durability proper

**D2a — replace transactional storage with a direct per-invocation handle.** Delete
`storage-host/src/transaction.rs` and `gc.rs`; **not a pure deletion**. StorageTransaction
is the sole write path (public re-export at lib.rs:67), held by broker-host storage
(:69/:91/:120) and threaded through its linker (lib.rs:957). Replace it with a plain
namespace + VFS handle applying each write directly: no overlay, commit point, recovery
or GC. Keep method names used by `broker-host/src/storage.rs`, including
`vfs_monotonic_time_ns`, `vfs_wall_time_ms` and the result of grepping
`StorageTransaction::` in broker-host; that file changes only the handle type name.
Remove `GcReport` (lib.rs:65). Keep namespace, layout, quota, vfs, key, jsonl, config,
metrics: ownership, derivation, binding and filesystem isolation are containment.
**Authorized public API change:** remove StorageTransaction/GcReport and export the
replacement handle. Crates: storage-host, broker-host. Delete overlay/commit/recovery
tests with their mechanism. Broker-host tests asserting visible writes survive
unmodified; rollback/crash-recovery tests may be deleted only with their names in the
commit body. Preserve quota/path/grant isolation tests; re-express through the direct
handle if needed, without loosening assertions. The compiler and residue gate bound
low-effort work; this is not permission for a new storage framework.

**D2b — delete checkpoint.rs and required checkpoint configuration.** Remove mod/use
and every checkpoint interaction in brokerd lib.rs. Exact contract:

- Remove `checkpoint_path`, `checkpoint_lock_path` in both config structs
  (:49–50/:344–345), resolution (:560–562/:691–693/:715/:869–870) and errors
  (:931/:971). `checkpointPath` and `checkpointLockPath` become unknown keys. Preserve
  existing loader behavior: reject if already deny_unknown_fields, otherwise leave
  ignored and state that in the commit; do not add a new strictness policy.
- **D2b public API:** `dekopon_brokerd::run` returns `Result<(), BrokerdError>`
  instead of `Result<AuditCheckpoint, BrokerdError>`; remove AuditCheckpoint and
  CHECKPOINT_API_VERSION. Change the corresponding types in brokerd `tests/server.rs`
  (:19/:1058) and dekopond `tests/gateway.rs` (:446), and nothing unrelated. Those fixture
  type edits are pre-authorized test drift; removal of dead config fields in fixtures
  is mechanical, not an assertion change.
- Delete nonempty-audit-without-checkpoint startup reconciliation
  (`checkpoint.rs:266–300/:434`); start with any readable audit file. The chain's own
  verification/CLI remains D2c; do not duplicate that unit.
- Remove chart checkpoint keys/comments: values-pr-summarizer-linter :20–21,
  values.yaml :88, verify-init-permissions.sh :178–180, chart README :61/:63/:148–149/
  :155–156, deployment.yaml :21/:108/:198/:353, pvc-state.yaml :10/:18. Grep charts for
  checkpoint and leave no active references.
- Broker `FileAuditLog::checkpoint()`/`contains_checkpoint()` are expected temporarily
  unconsumed until D2c; this explicitly described dependency is not a new discovery.

**D2c — audit hash chain to append-only JSONL.** The chain protects against a root
attacker already holding the secrets; dropping it matches the threat model. Delete:

- `dekopon_broker::verify_audit_chain` (lib.rs:2707), AuditIntegrityError (13 sites),
  DEFAULT_MAX_AUDIT_RECORDS (:324), FileAuditLog checkpoint()/contains_checkpoint(),
  integrity hashing and the verification-only read/reconciliation path.
- **D2c public API:** AuditRecord loses previous_hash and record_hash; it keeps
  sequence (JSONL ordinal) and event. Readers tolerant of unknown fields may read old
  records, but nothing re-reads for integrity and no migration is built. Other removed
  public chain/verifier items in this bullet list are expressly authorized.
- `brokerd/src/audit.rs`, Command::Audit, AuditArgs/AuditCommand, execute_audit (:336),
  AppError::Audit (:377), audit-path validation (:226–229), and main.rs's audit verify
  test (:463–490). Delete audit_max_records/auditMaxRecords and consumers: config.rs
  :10/:316, tests.rs :1042, chart values/overrides/README (six), brokerd README (two).

Records still append, keeping private-file/credential-safe audit boundaries. Do not
retain integrity/replay-recovery code on decided non-goal grounds. D2d owns the broad
remaining documentation sweep, including security-model :206.

**D2d — durability documentation sweep.** Remove roadmap signed/exported checkpoint
milestone (:278) and checkpoint clauses (:23/:40). Sweep checkpoint, hash chain,
hash-linked, audit verify and auditMaxRecords in docs/*.md, root README, brokerd README,
and chart prose. Baseline counts: operations 10; broker-http 8 (D7a owns that document);
upgrading 4; security-model 4; development 3; architecture/design/observability/container-
image 2 each; root README 4; docs/README/dekopond/inference 1 each. Delete a sentence or
rewrite to append-only JSONL; never add formerly/no-longer history to current docs.
Historical CHANGELOG entries remain. Finish with no active residue:

```sh
grep -rn 'checkpoint\|hash-linked\|hash chain' docs README.md charts crates/*/README.md
```

Approximate removed production: 3.2k plus chain code, subject to actual measurement.

### D3 — Delete the runner and import-free host

Ship only the two daemon binaries, not a standalone operator/runner CLI. The explicitly
retained daemon auth/provider/probe subcommands do not authorize a general broker CLI.
Every runner subcommand goes: inspect, invoke, shell, prompt, broker, session, chat.
Keep published dekopon-shell; the out-of-tree console is in scope as D8.

**D3a — delete recorded-session surface.** Remove session list/show/replay and
`dekopon-agent/src/replay.rs` (**988 production / 1,266 total**, not harness), plus its
replay-only context/portable-test companions. **Authorized public API:** remove the
agent replay module and replay exports. Keep the live agent loop, mounted skills,
read_skill and improvement suggestions. Do not migrate replay to the console. Delete
only tests of the removed property; preserve live telemetry/containment invariants.
Crates: run and agent.

**D3b — preserve OTLP end-to-end CI, rewrite its driver AND queries.**
`examples/otel-traces/smoke-test.sh` and the otel-e2e job must run real brokerd + dekopond
and drive one local-transport turn. Keep the OpenObserve compose stack. Use Python 3
stdlib, no socat/nc/new tool. A turn needs a model: build a stdlib HTTP OpenAI-compatible
stub (`kind: openaiCompatible`) using gateway.rs's broker_config (:115), gateway_config
(:222), request (:409) fixture as reference. Return a bash tool call that really invokes
a provider, then a final answer. Local request: `{"subject":"…","channel":"dev","text":"…"}`.
The socket is 0600 and the response is one JSON line. Broker spawn reference is
`dekopond/tests/gateway.rs:565`; gateway spawn is :592.

Old queries for runner.command/runner.invoke do **not** carry over. Assert
**gateway.message, gateway.session, broker.invocation, provider.compile, provider.invoke**,
plus the existing trace/log correlation and credential/payload sentinel-redaction
properties. Retain time bounds, failed-process diagnostics and unconditional cleanup.
The broker fixture is real: testkit FakeBroker has no socket and cannot substitute.

**D3c — atomic runner + provider-host retirement (includes old D3d).** Needs D8a/D8b,
D3a/D3b/D3g. Delete both crates, their manifests/dependency entries, regenerate the root
lockfile, update release-crates.txt together, and remove the retired WIT mirror's workflow
paths/comparison. Runner tests (36/51 currently load a component) die with the runner;
none are migrated to reproduce a retired surface.

This unit also owns about 30 packaging/consumer sites across CI, cache-warm, Dockerfile,
release workflow, Homebrew generation/tests, image staging, and docs/container-image.md.
Keep daemon build/help/install/archive/image coverage intact; remove retired executables
and direct-runner instructions. Do not leave dead commands for D3f. The metadata gate
must pass with neither an orphan library nor a mismatched release list. D3e owns only the
replacement privilege gates; D3c removes calls to nonexistent crates atomically.

**D3e — privilege gates.** Rewrite the old three normal-dependency-tree greps at
ci.yml:111/:120/:129 for the two surviving binaries. The gateway must not link
`dekopon-(broker|broker-host|brokerd|http-host|storage-host|policy)`; the privileged broker
must not link `dekopon-(agent|shell|model|process|config)`. Use anchored package-name
matches so the unprivileged broker-protocol client is not falsely rejected. Preserve
these opposite-direction checks in CI and documented local validation.

**D3f — final retirement changelog and component ownership only.** After all deletions,
record the four retired crates in CHANGELOG and finalize docs/architecture.md and
docs/design.md ownership. Manifest/lock/release-list work has already landed in D3c,
D7a and D7b; it is not deferred here. No crates.io writes and **no cargo yank**. Published
versions remain available; retirement is not a recall. The repo had no retirement
procedure (`docs/roadmap.md` is 322 lines, not a retirement guide); this is the procedure.

**D3g — broker probe, wave 1.** Add `dekopon-brokerd probe --socket <path>` with its
already-linked protocol client; replace chart startup/readiness and any liveness probe
that calls `dekopon-run broker capabilities`. Perform a bounded authenticated protocol
exchange; absent, refused and wrong-server sockets exit nonzero. No component loading,
model, credential discovery, generic invocation, or generalized broker CLI. Test parsing,
success/failure exchange and rendered chart command. This is a prerequisite of D3c.

### D4 — Transports: keep all five

Keep Slack, Discord, WhatsApp, Telegram and local. No transport fan-out or removals.
Local remains the development-only client path for D8a.

### D5 — Shell: keep all text builtins

Keep sed, grep, cut, sort, uniq, wc, xargs, jaq, parser, lexer, interpreter and limits.
Filtering inside the sandbox is the containment story working. No optional deletion.

### D6 — Dedicated gateway UID and group-reachable broker socket

`docs/roadmap.md:290` calls this the change turning via/namespace attribution into real
isolation. **This is the highest-value item and exempt from §7 line-count/test-ratio
simplicity targets.** Two high-effort units; do not delay them behind deletions.

**D6a — daemon/protocol boundary.** Accept configured distinct peer UIDs instead of
brokerd startup's UnreachablePeerUid refusal (`lib.rs:128–133`), while preserving real
peer-credential mapping in server.rs:352. Support a broker-owned **0660** socket with
a shared IPC group, in a broker-owned directory traversable but not writable by that
group. Sites: brokerd socket.rs:21 (private-parent validation) and :109 (bind mode),
broker-protocol lib.rs:2009 (validate_socket_path). Preserve configured server UID
verification, symlink/type/ownership checks, no permissions for others, bounds and
unmapped-peer refusal. Group access permits a connection, not an identity grant.
Private credentials/config/provider/storage paths remain private and broker-owned.
The local **chat** socket stays 0600 and development-only.

**Authorized API changes:** these socket/parent-mode and configured-peer contracts,
removal of UnreachablePeerUid, and only a minimal socket-group setting if necessary to
realize the boundary. Crates: brokerd, broker-protocol, dekopond only if its already
existing broker.serverUid plumbing needs adjustment. Owner-only local clients must
remain valid. Do not add generic transport abstractions or weaken credential-file checks.

**D6a focused-repair exception (owner authorized).** The surviving
`framing_and_audit_failures_name_their_cause` fixture may send only the encoded oversized
`runCommand` frame's length prefix. Its `write_all` of a >128 KiB buffer is not atomic:
the server rejects the length before reading a body and can close between partial writes.
Preserve every existing wire-code, log-cause, bound and redaction assertion; neither drain
oversized broker payloads nor raise limits. This fixture correction and its validation
belong in the same amended D6a work commit, not a separate repair commit.

**D6b — chart, init ownership and docs.** Integrate after D6a in wave 0; it can prepare
independently against this boundary. Keep pod-level runAsUser **65532** unchanged:
`_helpers.tpl:208` hard-fails any other value for provider ownership. Set gateway's
**container-level** UID/GID distinctly; keep broker 65532:65532 and supply the shared
supplementary IPC group. Gateway config explicitly pins `broker.serverUid: 65532`;
broker maps the actual distinct gateway UID. Adjust init ownership separately for
gateway config/model credentials/state and broker config/provider secrets/storage.
Changing only runAsUser is insufficient: neither container may gain the other's private
files. Chart tests must use the actual rendered init commands and permission layout.

Rewrite single-UID/committed-direction caveats in security-model.md :50/:137/:143/:206/
:214/:330–332 and roadmap.md :283–284/:290, with one canonical current-boundary statement
and references, plus corresponding chart/deployment prose. No unrelated chart redesign.

**Acceptance:** demonstrate a real cross-UID OS-process connection; server-UID pinning;
unmapped-peer and wrong-server refusal; gateway inability to read broker credentials
or write its private config/storage. Render/test the chart's actual UID/GID/init layout.
If unprivileged macOS cannot switch UIDs, use the existing container/chart facilities.
Skipped cross-UID proof is missing verification, not a pass. The negative-test answer is
that the deployment cannot run both daemons as one UID; that is the intended improvement.

### D7 — Remove web UI and operator CLI, preserve credential acquisition

**Keep ChatGPT model/auth implementation:** chatgpt.rs is **1,661 production / 1,246 test**
lines. It is the owner's only model-credential path, not a deletion candidate.

**D7a — webui plus brokerd embedding.** Remove dekopon-webui, chart broker.httpBind,
brokerd's embedded listener/view machinery (`src/lib.rs:107–507`) and its matching config,
CLI, tests and docs. **Authorized API:** removal of the web view/listener configuration
and embedding surface. Crates: webui, brokerd; atomic manifest/lock/release-list retirement.
This is not a standalone crate deletion: brokerd consumes it. Own docs/broker-http.md
and its links too: delete the obsolete document as the D2d plan anticipates, but preserve
any surviving HTTP/storage security invariants at their canonical surviving home before
repairing links. Do not delete actual provider HTTP functionality or its boundary tests.

**D7b — auth relocation, then standalone CLI retirement.** Before deleting dekopon,
move **auth chatgpt {login,status,logout,export} to dekopond auth chatgpt** in this same
unit. Reuse model library functions and CLI auth.rs; the daemon already holds the model
credential. Auth must not start transports or load gateway configuration. Preserve
isolated credential storage, device flow and safe diagnostic behavior. Update
`docs/chatgpt-credential.md:3–7` and chart seed-once export instructions.

Then delete standalone get/describe/validate/config, the crate and its CLI tests, and
examples/local/. CI uses the CLI at ci.yml:100/:567–661 and cache-warm.yml:157, not only
:577. Remove all obsolete consumers, manifests, lock/release entries atomically. Inspect
actual examples/local readers before deletion: preserve surviving catalog/skill test
invariants through remaining fixtures; do not mistake CLI-only claims for proof.
**Authorized surface/test migration:** new gateway auth subcommand, retired CLI surface,
and mechanical relocation of surviving example fixtures/tests. No model-client rewrite
or unrelated gateway refactor. This includes an auth move, not just pure deletion.

CI/charts/examples are in scope **only** for the named units' required consumers. All
transports and all shell builtins stay.

### D8 — Console on published 0.11.1, before runner deletion

The seams are in **crates/dekopon-tui/src/**: app.rs is a no-TTY event/key state machine,
run.rs restores the terminal on all exits including panic, redact.rs does rendering-time
redaction and terminal-control sanitization. Reuse **three** seams; record.rs observes
capabilities and is irrelevant to chat mode. Keep all rendering strings sanitized.

**D8a — chat mode.** A socket client of running dekopond's local development transport,
replacing run/src/chat.rs. One JSON request line and response line over a 0600 socket;
no component, host imports, model credential or tool loop. The caller declares a subject,
so show the development-only/owner-reachable warning in the UI, not just docs. Prove
bounded/error/disconnect handling and terminal-safe rendering without a TTY.

**D8b — start the existing shell without resolving a model credential.** The shell
prompt already exists at `crates/dekopon-tui/src/ui/shell.rs`, using dekopon_shell's
Interpreter and a broker leg. **Do not build another REPL.** Keep proposal/policy/exit/
pipeline rendering; make shell-only startup avoid model credential files, model credential
environment resolution and model endpoints. Turn mode still requires its credential.
Test absence of credential access, not just a happy path with an available credential.

Console work is in scope but a separate repository/PR. Integrate/review/test/push D8
before D3c. Later core publication and console re-pinning are follow-ups, not authorized
operations or blockers for this run.

## 5. Execution model and gates

No broad audit or decision phase. The prior scrub's +12,136/−12,178 (net −42) optimized
tidiness rather than removal. Spend agents on bounded implementation and fresh review.
One worker per unit, one commit per unit, in isolated worktrees branched from that repo's
integration branch. Git commands always use `git -C <path>`. Never integrate while a
gate is running in the integration worktree. Cherry-pick worker commits and gate after
every batch, catching semantic as well as textual conflicts.

**Sizing:** a unit exceeding about 800 production lines or three crates reports a split
proposal instead of improvising past its contract. Whole-file/crate deletions explicitly
named above are already sized deletion contracts; do not count erased lines as newly
implemented state. Retained/new logic still needs to fit the unit and crate bounds.

| Unit | Owner scope | Effort |
|---|---|---|
| D6a | daemon/protocol distinct-UID socket boundary | high |
| D6b | chart/init ownership, cross-UID deployment proof, docs | high |
| D1a | Slack single 429 retry | medium |
| D1b | policy-world and transport aggregate refusals | medium |
| D8a | console local chat client | medium |
| D8b | existing console shell credential-free startup | high |
| D3a | runner session surface and agent replay deletion | low |
| D3b | two-daemon OTLP driver, stub model and queries | medium |
| D3g | broker probe and chart consumers | medium |
| D2a | direct storage handle, transaction/GC removal | low |
| D2b | checkpoint removal, config and fixtures | low |
| D7a | webui removal including brokerd embedding | low |
| D7b | auth relocation, operator CLI removal | low |
| D2c | append-only JSONL, chain/verification removal | high |
| D3c | run + provider-host atomic retirement and packaging | low |
| D3e | opposite-direction daemon dependency gates | low |
| D2d | remaining durability docs | low |
| D3f | final retirement changelog/component docs | low |

D3d is folded into D3c; there is no D1c slot. Every reviewer and the orchestrator use
high effort, not max. D2a/D2b get especially close initial scope-drift scrutiny before
trusting the later low-effort deletion handoffs.

**Ledger:** `SIMPLIFY.md` at each repo root records each unit as pending/in-progress/
landed/blocked, its commit identity and one line for discoveries not fixed. Update the
unit's entry in the same work commit. A commit cannot contain its own SHA: record its
unique unit trailer/ref in that commit, and resolve it to the exact SHA at the next
integration update (without a separate drifting ledger-only commit). Only the owner of
an entry edits it; integration preserves other units' entries. Keep the ledger through
validation and remove it in the final housekeeping commit before the PR, as the original
brief requests. The amended brief stays committed.

### Per-unit mechanical gate

The non-Rust residue check is `.github/scripts/check_simplify_residue.py`; its regression
tests are `.github/scripts/test_check_simplify_residue.py`. Install and pass these
**before any low-effort deletion worker relies on the check**. Run it with `--repo <root>
--symbols <inventory-file>`, one literal removed symbol/path per line. Exit 0 means no
hits, 1 reports residue, and 2 is a scanning/setup error; never hide either nonzero status.
No worker may waive a gate. Evidence must identify the exact unit commit.

1. `cargo check -p <each touched surviving crate> --locked`.
2. `cargo test -p <each touched surviving crate> --locked`. Fetch the checksum-pinned
   ignored external provider fixtures first when tests need them. For deleted crates,
   validate surviving consumers/metadata; do not cargo-test a nonexistent package.
3. **Test drift:** diff only Rust tests, not `'*test*'` over every file extension. Include
   Rust integration-test directories, Rust files whose names contain test, and inline
   test sections in changed Rust modules. Use `git diff --diff-filter=M HEAD~1` for
   modified-file assertion checks; wholly deleted test files are not noise findings.
   Pure deletion may delete tests of deleted properties; modified surviving tests need
   a named unit-contract allowance or a report/stop. D2a/D2b/D7b's exact migrations above
   are not permission to loosen assertions. A path such as testkit/README.md is not a test.
4. **No new suppressions:** additions matching
   `^\+.*#!?\[\s*(ignore|allow|expect)\b` must be empty unless explicitly authorized by
   the unit contract. This includes #[expect( and #![allow, not only #[allow(.
5. **No loosened assertions:** inspect removed assert lines using
   `git diff --diff-filter=M HEAD~1 -- '*.rs'`. Every removed assertion belongs to a
   wholly deleted test/property or an explicitly re-expressed invariant; no weakened
   assertion is accepted. Excluding deleted files removes the prior gate's 218-hit noise.
6. **Non-Rust residue:** after every deletion, grep the repository, excluding target/,
   for **every removed public identifier, crate name, binary name and file path** across
   .md/.yaml/.yml/.sh/.py/.toml and all .github files. Keep the identifier inventory and
   raw hits with the unit evidence. Zero hits, or report the hits and stop that handoff;
   the compiler cannot see dead CI invocations, chart fields or current documentation.
   Historical CHANGELOG entries and this execution brief/ledger necessarily name the
   removals: identify them explicitly, never rewrite history or pretend they are zero.
   The contracts expressly describe D2c's temporary dead methods and D2d's later prose
   sweep; report those named future-owned hits to orchestration rather than silently
   widening an earlier worker's scope. Any unowned/current executable residue is a gate
   failure. No undocumented allowlist or general exclusion of docs/CI is allowed.
7. **Negative test:** one sentence in each commit body: what can this codebase no longer
   do? Nothing is a valid answer; any lost capability must name the approved decision.
8. Run git diff --check, formatting, doc/script/metadata gates appropriate to the
   changed surface. A deletion's release metadata must be internally consistent now,
   not only after a later orphan cleanup.

### Exactly four stop-and-ask triggers

Halt and report when: (1) a deletion changes a public API **not named in its unit's
contract**; (2) a surviving real test invariant cannot be re-expressed; (3) a gate fails
for a reason this brief does not describe; or (4) a unit depends on another unit that
has not landed. Do not interpret ordinary authorized public removals as trigger (1).
A runtime/tooling infrastructure failure is also reported exactly under the harness
protocol, with clean/partial worktree state; no silent change of execution mode.

### Disk hygiene — follow the schedule, never wait for disk-full

The global Rust wrapper is /opt/homebrew/bin/sccache via ~/.cargo/config.toml. If missing,
repair with brew install sccache, never bypass it. Cache authority:
~/Library/Application Support/Mozilla.sccache/config. Do not set per-agent SCCACHE_DIR,
SCCACHE_CACHE_SIZE, CARGO_TARGET_DIR, build.target-dir, or CARGO_INCREMENTAL=0; never
change wrappers or use cargo clean. Every worktree has its own default target/.

1. **Before every wave/batch:** df -h /, git -C <repo> worktree list, du -sh relevant
   targets. Launch N build workers only with roughly **more than N × 30 GB** free.
   Reduce width/run halves as needed. Six-wide is not a requirement to exhaust disk.
2. **Every worker handoff:** once its commit is integrated and no cargo/rustc or running
   executable uses its exact ignored/rebuildable target/, remove that target before
   issuing the next brief. Check processes/cwd/open files and path/ignored status first.
3. **Every wave boundary:** remove clean, inactive, integrated worker worktrees using
   git -C <repo> worktree remove <path>, then worktree prune. Never rm -rf a registered
   worktree. Preserve sources/branches/dirty or unmerged work and other agents' artifacts.
4. Prefer package-scoped Cargo while iterating; workspace --all-features is an
   integration gate, not a per-unit build multiplier. Record target growth and physical
   df before/after cleanup. Retain the integration target only for identified next gates.
5. After exact-head validation/CI and no planned builds, remove inactive ignored
   integration/provider-workspace targets before final handoff too.

Under pressure inspect cargo/rustc and registered worktrees first, then remove only
inactive reproducible targets. Preserve the shared bounded cache; sccache --show-stats
is inspection only. Follow ~/.pi/agent/skills/free-up-worktree-space/SKILL.md.

### Wave order

A wave ends only when its units are integrated and the integration gate is green. Work
inside a wave is parallel where independent and disk permits; shared-file semantic
integration is serial. D6b integrates after D6a; no other same-wave dependency is invented.

| Wave | Units | Dependencies / boundary |
|---|---|---|
| 0 | **D6a, D6b**, D1a, D1b, **D8a, D8b** | D6 first priority; D6b validates D6a; console separate repo on 0.11.1 |
| 1 | D3a, D3b, **D3g**, D2a, D2b, D7a, D7b | independent contracts; split by actual disk headroom; inspect D2a/D2b scope closely |
| 2 | D2c, D3c | D2c needs D2b; D3c needs D8a/D8b, D3a/D3b/D3g; provider-host goes here too |
| 3 | D3e, D2d | after D3c and D2c; no D3d orphan interval |
| 4 | D3f | changelog/component prose only, after atomic crate retirements |

Use one static subagent workflow for these bounded units, not broad rediscovery. The
old proposed dynamic replay-test/transport fan-out is obsolete: replay tests are deleted,
not migrated, and all transports stay. One fresh adversarial reviewer per unit commit
at its boundary; do not advance with material findings unresolved. Allow a focused
same-worker repair and re-review, not an end-of-run repair bucket. Report usage between
milestones and keep durable output references, not copied transcript piles.

## 6. Reviewer instructions

Fresh context, read-only, high effort; one exact commit and its contract. An unearned
pass is worse than a wrong objection. Report evidence, not just conclusions.

- **Prove absence.** Independently check consumers, including indirect Serialize,
  Debug and derived PartialEq readers; identifier-only grep is insufficient. Report
  exactly what was checked. Review the non-Rust inventory/residue too.
- **Do not weaken tests.** A test of a property removed by this decision may go. A real
  surviving invariant exercised through a removed mechanism must be re-expressed.
  Loosened assertions are not a refactor. Named contract migrations are behavior changes
  and must say so; no-behavior-change claims need an unmodified passing suite.
- **Enforce scope.** Reject adjacent fixes, speculative abstractions and broadened
  privilege. Record outside-scope discoveries, do not fix them in a unit.
- **Reject decided non-goal retention arguments on sight:** idempotency, exactly-once,
  reconciliable state, crash durability. Preserve credential/identity/path boundaries.
- Answer the negative test for each commit; review D6 on the real OS boundary, not LOC.

## 7. Simplicity benchmarks

Structural rules: no trait with one production implementation; no versioned format
without a second reader; no state field without a reader; no abstraction consumed only
by its own tests; no name implying properties absent from code (an in-memory checkpoint
or journal must not imply durability). Do not expand unrelated scope to clean the world.

Report before/after in the PR body:

| Metric | Pinned baseline | Method |
|---|---|---|
| Production Rust lines | 74,903 | §2 script |
| Test Rust lines | 57,386 | total minus production |
| Crates | 26; target 22 | directories / Cargo workspace metadata |
| Public API items | measure | cargo public-api or rustdoc JSON |
| Longest Rust file | dekopond/src/tests.rs, 9,879 | find + wc + sort |
| Workspace dependencies | measure | cargo tree --workspace --depth 1 |

Test lines must fall at least proportionally to production lines; leaving tests of
removed mechanisms is not simplification. **D6 is exempt** from production/test-ratio
and negative-capability simplicity judgments: added code/tests proving distinct-UID
containment are the point. Report its additions separately, not as hidden deletion
regression. Every removed capability still matches a settled decision.

## 8. Integration, final gate and PRs

Rust-only green is insufficient. Run the applicable exact command groups in
.github/workflows/ci.yml **and** .github/workflows/wit-package.yml, preserving required
check names and recording exact-head evidence. After each integration batch, run
surviving touched-package tests plus non-Rust/doc/metadata gates; at each full wave
boundary run the integrated workspace gates. Final validation includes:

- cargo fmt --all -- --check; git diff --check;
- cargo clippy --workspace --all-targets --all-features --locked -- -D warnings;
- cargo test --workspace --all-features --locked --no-fail-fast, plus --doc;
- RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked;
- cargo machete, cargo deny, both opposite-direction normal-dependency boundary checks;
- surviving daemon release-profile checks, feature-off core/capability/protocol checks;
- every provider-example workspace's fmt/clippy/test and wasm32-unknown-unknown check,
  plus guest SDK/HTTP/storage binding feature variants from CI;
- actionlint workflows, shellcheck repository scripts, documentation duplicate and
  audit-event checks, verify-release-metadata.py and its tests, package metadata/archive
  gates selected by CI, daemon help/install smoke, rewritten OTLP smoke, chart tests;
- MSRV cargo +1.89.0 test --workspace --all-features --locked --no-run and --doc;
- all six provider build.sh deterministic byte comparisons when their inputs change
  (including core/provider-http/provider-storage); never hand-edit wasm or lockfiles.
  WIT package gates additionally retain local package/mirror/round-trip checks.

Full Linux/container-only acceptance may be proved by exact-head CI, never asserted
passed without observing it. Do not mark a PR ready with missing validation. One core
PR against main with one commit per unit plus the required brief/final-housekeeping
commits; one console PR for its two units. Before/after metrics and the required
Summary / Security impact / Validation / Limitations sections go in core PR. No live
cluster changes, tags, releases, crates.io writes or merges. Console re-pin is follow-up.

## 9. Correction application record

The former unapplied addendum is incorporated in §§2–8: corrected replay/gateway/CI/
security citations; model-backed OTLP queries; two D1 port slots; explicit public API
contracts; D3g; D3d folded into D3c; D3f reduced; D6a/D6b; auth relocation; existing-shell
credential-free startup; corrected test/suppression/assertion and non-Rust residue gates;
22-crate target and 9,879-line maximum. This section contains **no overrides**. The
unreviewed parts of the old wave/reviewer/final-gate text are not represented as verified.
