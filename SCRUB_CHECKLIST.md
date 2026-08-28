# Scrub Checklist

**Scope:** whole `dekopon` workspace — 26 crates, ~123k LOC of Rust, plus `charts/`, `ci/`, `.github/`, `examples/`, `docs/`, `wit/`. `rpi-homelab` was read for deployment context (what is actually set on the Pi), not audited for its own smells. 12 read-only auditors, one per category group; raw reports are in vagus as "Dekopon scrub 2026-08-27 — raw report A1…F".
**Baseline:** builds clean (`cargo build --workspace --all-targets`); `cargo test --workspace --no-fail-fast`: **1234 passed, 0 failed, 0 ignored** across 81 test binaries. Precondition: run `ci/fetch-external-provider-components.sh examples/providers` first — without the three fetched fixtures, 17+ tests fail with `NotFound` on `examples/providers/{echo,jsonplaceholder,memory-chat}-provider.wasm`.
**Branch / HEAD at audit:** main @ a1f3cc896b0c2eaa37815935e0db544934d68c01 (tree clean; the fetched fixtures are gitignored)

35 findings below (58 raw auditor findings merged), sorted by blast radius then confidence (HIGH-blast items first; the MEDIUM-blast items kept are the ones that need an owner decision or are one-commit wins). 45 more reported findings cleared the bar but were not promoted (OVER_ABSTRACTION 8, WRONG_ABSTRACTION 9, PREMATURE_GENERALIZATION 6, PHANTOM_CONFIGURABILITY 5, TEST_THEATER/BLOAT/TAXONOMY 8, INCONSISTENT_IDIOM 2, VERSION_DRIFT_RISK 2, REINVENTED_WHEEL 2, DUPLICATED_LOGIC 1, SIDE_QUEST 1, misc 1), and the auditors themselves cut a further 68 for length (in each raw report's "Dropped" section) — they are listed one line each in the vagus note "dekopon codebase scrub 2026-08-27 — findings and resume instructions" §Tier 2, and any can be promoted by appending an entry here with the next number. DEAD_CODE / COMMENT_ROT (auditor D1) reported last; its four entries are appended as #32–#35 after the sorted list.

## How to respond

Edit the **Decision:** line of each entry. It must begin with one of:

- `APPROVE` — do the suggested fix as written
- `SKIP` — don't do it, don't ask again
- `DEFER` — revisit in a later scrub; stays in this file, not executed now
- `MODIFY: <instruction>` — do a changed version. Your instruction replaces
  the "Suggested fix" text entirely, e.g.
  `MODIFY: only in src/api, leave the worker path alone`

Anything else — blank, or prose without a leading token — counts as
undecided, and you'll be asked about it again. The checkbox is not a
decision; it's the progress marker the scrub ticks off while executing.

## Execution notes for pass 4

- The primary checkout is pinned to `main` by a post-checkout hook. Pass 4 must work in a linked worktree: `git worktree add -b scrub/<topic> .worktrees/scrub-<topic> main`. Never `git switch` in `~/code/dekopon/dekopon`.
- Run `ci/fetch-external-provider-components.sh examples/providers` in the worktree before the first `cargo test`.
- One commit per finding number: `fix: <what> (#<n>, <CATEGORY>)`. Findings that touch `rpi-homelab` (#10) are a second PR in that repo.
- Implementation goes to opus agents; this file plus the cited raw report is the whole brief an agent needs.

## Recommended fix order

Decided 2026-08-27: 25 APPROVE, 7 MODIFY, 3 DEFER (#15, #16, #26), 0 SKIP. Consequences for ordering: #16 deferred means the crate count stays at 26 and #14 still targets `dekopon-provider-sdk`; #26 deferred means #31 covers all four transports; #15 deferred means #20 works on the unshrunk surface; #22 now bundles #32 and the console-doc half of #33.

- **Wave 0 — trivial, independent, do first:** #1, #28, #2, #3, #9, #33 (docs only).
- **Wave 1 — small correctness / diagnosability, independent:** #4, #5, #6, #7, #8, #18, #10 (+ rpi-homelab PR), #34, #35, #21 (error collapse only), #24, #30.
- **Wave 2 — scope changes. Do before the refactors so the refactors are smaller:** #22 (console out of tree, carrying #32 and the console-doc strip from #33), #25 (chart wiring), #27 (collapse), #23 (`-E`).
- **Wave 3 — structural, in this order:** #19 (shared test support) → #17 → #13 (file-hygiene helper) → #14 (shared Wasm-host plumbing into `dekopon-provider-sdk`) → #29 (telemetry install) → #31 (transport helpers, all four transports) → #11 (dekopond validation — after #27 shrinks `config.rs`) → #12 (typed memory route) → #20 (BrokerRequest collapse — last and largest; #12 shrinks its surface first).
- **Deferred to the next scrub:** #15 (secrets backends), #16 (process crate), #26 (WhatsApp/Telegram).

---

## [x] #1 — SIDE_QUEST — .gitignore:23

**Finding:** `.gitignore` still ignores `/examples/pr-summarizer-linter/broker-credentials.yaml`, a directory deleted in #155; the surviving walkthrough `examples/conditional-write/README.md:49-57` tells the reader to paste a live token into `broker-credentials.yaml` and asserts "it is in `.gitignore`". `git check-ignore examples/conditional-write/broker-credentials.yaml` exits 1. (raw: B1)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Change line 23 to `/examples/conditional-write/broker-credentials.yaml`; verify with `git check-ignore`. Then grep `.gitignore` and `charts/dekopon/README.md:197,612,617` for other `pr-summarizer-linter` residue from #155.

**Decision: APPROVE**

## [x] #2 — VERSION_DRIFT_RISK — crates/dekopon-model/src/chatgpt.rs:90 (+ :301, image.rs:135, model.rs:395)

**Finding:** All four `ureq::Agent` builders inherit ureq's default `proxy: Proxy::try_from_env()`, so the ChatGPT bearer token, the OAuth device-flow exchange, and every prompt follow an ambient `HTTPS_PROXY`/`ALL_PROXY` — while `dekopon-http-host` (same binary) explicitly sets `.no_proxy()` and `docs/architecture.md` states "disables ambient proxies" as a global property. (raw: E)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Add `.proxy(None)` at all four sites and hoist them into one `fn agent(timeout) -> Agent` in `dekopon-model` so the stance lives once. Do not switch stacks in this commit (folding onto `reqwest::blocking` is a separate, API-breaking decision — see Tier 2).

**Decision: APPROVE**

## [x] #3 — SILENT_FAILURE_MODE — crates/dekopon-model/src/chatgpt.rs:1209

**Finding:** The ChatGPT credential-path resolver is the only one of the three XDG/HOME ladders without the empty-value filter the other two carry (`dekopon-config/src/lib.rs:411`, `broker-protocol/src/lib.rs:2309`), so `XDG_CONFIG_HOME=""` resolves to the *relative* path `dekopon/auth.json` and `save_credentials` writes the OAuth refresh token (0600) into the process cwd — a git checkout, typically — with no error; the next run from another cwd re-prompts login. (raw: E, D2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Add `.filter(|v| !v.is_empty())` to the four `env::var_os` reads in `resolve_auth_path_named` (`XDG_CONFIG_HOME`, `HOME`, `APPDATA`, `DEKOPON_CHATGPT_AUTH_FILE`), and reject a relative result with the existing `ChatGptError::Configuration("… set DEKOPON_CHATGPT_AUTH_FILE")`. Do not merge the three ladders (they differ on probing and tiers by design).

**Decision: APPROVE**

## [x] #4 — SILENT_FAILURE_MODE — crates/dekopond/src/session.rs:148-150 (+ :109, model.rs:403)

**Finding:** `api_key_env` is read with `std::env::var(name).ok()`, so a missing, non-UTF-8, or exported-but-empty model API key becomes "no bearer token" and the gateway starts clean, then 401s on the first user message; `ModelCache::client` caches that tokenless client for process life, so exporting the key later needs a restart — contradicting its own docstring at `session.rs:180-184`. Every chat-transport credential in the same file fails closed at startup via `transport::read_credential`. (raw: D2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Route `session.rs:148-150` and `:109` through `transport::read_credential`/`credential_value`, mapping `Missing/Empty/NonUtf8Credential` into the local error enums (add `EmptyCredential` to `ImageGeneratorStartupError`). Preserve: `api_key_env` absent stays `None` (loopback llama.cpp needs no key); present-but-unset becomes a startup error.

**Decision: APPROVE**

## [x] #5 — SILENT_FAILURE_MODE — crates/dekopon-broker/src/lib.rs:3367 / :3418 / :3304

**Finding:** `resolve_chat_claim` returns four refusal classes (`attestation-denied`, `unmapped-subject`, `agent-denied`, `policy-error`); three of its four callers discard them for a hardcoded `"chat-attestation-denied"` with empty `policy_ids` — on the chat path that carries all four live transports — and `:3304` returns `UnknownCommandWord` with no audit record and no `report_inspection_refusal`. Both discarding sites call the helper twice, so every chat invocation runs Cedar twice. The non-chat path (`:3197`) preserves both fields. (raw: D2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Call `resolve_chat_claim` once at `:3364` and `:3415`, bind the `Result`, build `Refusal { reason, policy_ids }` from `Err`; have the helper return the `PolicyDecision` alongside the reason so `determining_policy_ids` survives; at `:3304` call `report_inspection_refusal` before returning. Preserve: the *wire* answer stays opaque (the `map_err_ignore` allow at `:3476` is correct) — only audit/telemetry change.

**Decision: APPROVE**

## [x] #6 — SILENT_FAILURE_MODE — crates/dekopon-storage-host/src/transaction.rs:852

**Finding:** `Err(_) => self.post_marker_failure()` collapses four causes (`Arithmetic`, `Timeout`, `QuotaExceeded`, filesystem I/O incl. `ENOSPC`) into the fieldless `StorageHostError::OutcomeUnaudited`, in a crate with zero `tracing::` calls; it becomes `BrokerError::StorageOutcome`, the only variant in that enum with no `#[source]`, constructed at `broker/src/lib.rs:4483` with no log. An operator facing a poisoned namespace cannot tell "free disk" from "raise the quota". (raw: D2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** `Err(source) => self.post_marker_failure(source)`; carry a coarse cause discriminant on `OutcomeUnaudited` (reuse the `public_reason` vocabulary); add `#[source]` to `BrokerError::StorageOutcome`; emit one `tracing::error!` with `error_chain` at `broker/src/lib.rs:4483`, as the audit path already does. Preserve: the guest still receives the opaque mapped WIT error.

**Decision: APPROVE**

## [x] #7 — SILENT_FAILURE_MODE — crates/dekopon-webui/src/listener.rs:50-94

**Finding:** The webui accept loop logs `webui_accept_failed` with no error field and retries forever at 1 s on `EBADF`/`EINVAL`, while the broker's own accept loop 400 lines away (`brokerd/src/server.rs:145-158`) classifies errno via `retryable_accept_error`, logs `error_chain`, backs off 100 ms→1 s, and aborts on non-retryable errors. A third fixed-100 ms copy lives in `dekopond/src/transport/whatsapp.rs:55`. (raw: D2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Move `retryable_accept_error` + the two backoff constants to a shared home (`dekopon-core`, or `dekopon-webui` if the graph forbids `webui → brokerd`), have both loops use it, and emit `error = %error_chain(&source)` from both. Preserve webui's no-sleep `continue` on genuine per-connection aborts (`ECONNABORTED`/`ECONNRESET`).

**Decision: APPROVE**

## [x] #8 — OVER_ABSTRACTION — crates/dekopon-shell/src/lib.rs:147-237 (CapabilityInvoker)

**Finding:** `CapabilityInvoker` has 7 defaulted methods and 4 hand-written forwarders; three of them (`dekopon-tui` `RecordingInvoker`, `LegHandle`, `dekopond` `CancelAwareInvoker`) do not forward `invoke_with_secret_use`, so the default fires and #173's secret-DRN path is unreachable from both `dekopond` and `dekopon console` — a broker-backed session denies with "secret references require a broker-backed capability". `curl.rs:45` produces the proposal; it never leaves the process. (raw: A3)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Make the composable methods required (keep defaults only for leaf-meaningful `describe`/`resolve_command`), or split into a `CapabilityLeg` trait with a blanket forwarding impl so `LegHandle` disappears; fix the three forwarders and add one test per forwarder that a DRN proposal reaches `BrokerLeg`. Preserve deny-by-default on the *direct* leg (`RegistryInvoker`, `NoDirect`).

**Decision: APPROVE**

## [x] #9 — SILENT_FAILURE_MODE — .github/workflows/ci.yml:54, :129 (+ .github/scripts/classify_ci_changes.py:26-32)

**Finding:** Both documentation gates (`check_docs_duplicates.py`, added by `a1f3cc8` itself, and the audit-event-documented check) live inside `quality_checks`, which is gated on `run_rust != 'false'`, so a docs-only PR skips exactly the checks that police docs and shows green. `check_docs_duplicates.py` and `render-homebrew-formula.py` are also absent from `FULL_CI_INPUTS`, so editing the gate script runs zero jobs. The gate runs on `docs` only, not `README.md`/`AGENTS.md`/`crates/*/README.md`. (raw: F)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Add a `run_docs` classifier category (`docs/**`, `**/*.md`, the two scripts) and a cheap toolchain-free `docs` job carrying both steps; add the two scripts to `FULL_CI_INPUTS`; widen the target to the READMEs and `AGENTS.md`.

**Decision: APPROVE**

## [x] #10 — PHANTOM_CONFIGURABILITY — crates/dekopon-brokerd/src/config.rs:129-131 (brokerLimits / hostLimits)

**Finding:** `brokerLimits.maxReplayIds` (default 100 000) is authored nowhere — not the chart default, not `rpi-homelab` — while `docs/broker-http.md:139`, `charts/dekopon/README.md:351`, and `brokerd/README.md:486` all instruct setting it equal to `auditMaxRecords: 200000`, which the live Pi broker does set. The live replay ledger will hard-fail at 100 000 decisions with `capacity-exhausted`. `hostLimits` is 14 required fields (all-or-nothing), so nobody sets `maxTotalMemoryBytes` either, and the config comment at `config.rs:238-246` says the unset worst case exceeds a small container's limit. (raw: B2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Per-field `#[serde(default)]` on `BrokerLimits` and `HostLimitsConfig`; set `brokerLimits.maxReplayIds: 200000` and `hostLimits.maxTotalMemoryBytes` in `charts/dekopon/values.yaml`'s default inline config; separate PR in `rpi-homelab` setting the same in `manifests/dekopon/config/broker.yaml`. Preserve the cross-field validation in `resolve()` (`config.rs:826-863`).

**Decision: APPROVE**

## [x] #11 — INCONSISTENT_IDIOM — crates/dekopond/src/config.rs:745 (+ routes.rs:64, lib.rs:195-224)

**Finding:** `dekopond::config::resolve` has 32 first-error returns and `RoutingTable::bind` five more, while `dekopon-config` next door aggregates every problem and `docs/design.md:203` mandates it. Worse, `lib.rs:195-224` reads each transport's credential and opens its socket inside one loop, so a rollout missing two tokens costs two crash-loops, each having already authenticated to Slack. `brokerd/src/secrets.rs:1043` aggregates; `brokerd/src/config.rs` does not. (raw: A1)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Give `dekopond` the `ConfigError::Invalid { problems }` shape from `dekopon-config`: `resolve` and `bind` push into a `Vec`, merged into one refusal; split credential *presence* out of `build_transport` into a pre-flight pass over all transports before any `connect()`. Preserve `dekopon-config`'s `incomplete` precedent — skip dependent reference checks when the referenced list itself failed.

**Decision: APPROVE**

## [x] #12 — WRONG_ABSTRACTION — crates/dekopon-broker/src/lib.rs:93-97, :1446 (+ ~30 decision sites)

**Finding:** The optional `memory-chat` provider is identified inside the authorization core by string-matching operator-chosen names (`capability.starts_with("memory.chat.")`, `provider == "memory-chat"`; 72 `MEMORY_*` refs; the same predicate spelled three ways at `:2978`, `:3025`, `:3311`). Naming any capability `memory.chat.export` or any provider `memory-chat` in `broker.yaml` silently changes what the broker hides and denies; renaming the shipped provider silently drops the reserved gate that keeps `record` off the generic invoke path. (raw: A2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Add one typed field to `ConstraintSet` (e.g. `route: CapabilityRoute::{Generic, ChatMemory{Record|Retrieve}}`) authored by the operator and validated once in `ConstraintCatalog::validate`; replace the string comparisons with matches on it; delete the three duplicate word predicates. Preserve: `record` unreachable from generic invoke, `memory` word unreachable outside chat scope, and the `with_chat_memory` startup composition checks (`:2765-2860`).

**Decision: APPROVE**

## [x] #13 — DUPLICATED_LOGIC — crates/dekopon-brokerd/src/config.rs:407 (+10 sites, 5 crates)

**Finding:** The "trusted file" predicate (`O_NOFOLLOW` + regular + `uid ==` + mode mask + `nlink == 1` + size cap + bounded read) is hand-written ≥11 times across 5 crates, and the mode mask silently differs — `0o077` (credentials, secret map, checkpoint, socket, storage keys) vs `0o022` (broker.yaml, dekopond.yaml, policies) — with nothing naming the two tiers; both refuse as `InsecureFile`. Only `provider_manager.rs:1706` and `layout.rs:305` parameterize the tier. (raw: D2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** One `read_trusted_file(path, uid, tier: FileTier::{Private, NotWorldWritable}, max_bytes)` in `dekopon-core` returning a structured `FileHygieneError` that callers map into their own enums; document the two tiers in its doc comment. Preserve `credentials.rs`'s `NotRegular`/`InsecureFile`/`TooLarge` distinction (do not spread `secrets.rs:1308`'s five-into-one collapse).

**Decision: APPROVE**

## [x] #14 — DUPLICATED_LOGIC — crates/dekopon-provider-host/src/lib.rs:427,150 vs crates/dekopon-broker-host/src/lib.rs:1102,419

**Finding:** The two Wasmtime hosts copy verbatim: `ProviderConflicts` + `Display`, `validate_manifest`/`invalid_manifest`, `validate_limits`, the registry conflict loop (comment included), seven identical `DEFAULT_MAX_*` constants, the `CacheConfig`/`Cache::new` block, and the five-call `StoreLimitsBuilder` chain; `HostLimits`/`BrokerHostLimits` repeat eight fields with the same doc text. The hard part (`command_word_conflicts`) was already extracted to `dekopon-core` — the extraction stopped halfway. Operator-visible text has already drifted; the #79 Wasmtime 36→40 bump must be reviewed twice. The crate split itself is correct and CI-enforced. (raw: A4, D2, E)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Move the shared validation layer, the seven constants, a common `StoreLimits` struct + `store_limits()`, and `fn engine(limits, cache_dir, async_support)` into `dekopon-provider-sdk` (both hosts already depend on it; CI's `dekopon-run` dependency gate does not forbid it). Pass the `EffectKind::ReadOnly` gate and the two `Display` remedy strings as parameters. Do not merge the hosts, the linkers, or the timeout machinery (sync epoch vs async fuel-yield is real). Re-export the seven `pub const`s from their old paths for one minor cycle (published crates).

**Decision: APPROVE**

## [ ] #15 — PREMATURE_GENERALIZATION — crates/dekopon-brokerd/src/secrets.rs:89-193 (+ :742-786, capability/src/lib.rs:394)

**Finding:** `SecretSource` ships ten backends (1Password Connect, Vault KV1/KV2, AWS Secrets Manager, AWS SSM, GCP, Azure Key Vault, k8s API, k8s projection, secure file) — eight are hand-written vendor HTTP clients tested against one mock each, nine have never resolved a real secret; the AWS pair hand-rolls SigV4 with no signature-vector test. `secretMapPath` is set in no deployment, example, or chart value; `constraints.secretUse`'s scope knobs `allowQuery`/`maxInjections`/`basicUsername` occur only in `docs/secrets.md` — zero tests. The live broker still uses `credentialsPath`, including for 1Password. (raw: F, B2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Keep `secureFile` + `kubernetesProjection`; delete the eight remote adapters, `read_aws_credentials`, the SigV4/`hmac`/`crc32c` helpers and the `time`/`hmac` deps they pull into brokerd (keep the `SecretSource` enum shape so a backend returns as one variant). Add one end-to-end brokerd test authorizing through `secretUse` with `allowQuery: true`, `maxInjections: 2`. Until a deployment sets `secretMapPath`, mark the feature **Exploration** in `docs/design.md` (or wire the live `github-pat` through one `secureFile` entry in `rpi-homelab` and keep it Current).

**Decision: DEFER**

## [ ] #16 — CRATE_FACTORIZATION — crates/dekopon-process/ (whole crate)

**Finding:** A 26th published crate (309 src + 348 test LOC) whose `Process` trait has one impl (its own `ProcessFn`) and one call site (`dekopon-run/src/lib.rs:389`, kind string `"legacy-shell"`); its abandonment-observer machinery — the supervisor double-spawn, `OutcomeEnvelope`, three of six tests — exists for a dropped future that the only caller cannot produce (no `select!`/`timeout`/`abort` above it). Five sibling handoff sites in `dekopon-run`, `dekopon-tui`, `dekopond` still roll their own `spawn_blocking`. `docs/design.md:329` and `roadmap.md:227` both forbid crates without a consuming milestone. (raw: B1, A3, E, F)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Move `lib.rs` to `crates/dekopon-run/src/process.rs` as a private module; delete the crate from `[workspace.members]`, `[workspace.dependencies]`, `.github/release-crates.txt`, and its design/roadmap paragraphs; drop the `Process` trait/`ProcessFn`/`on_unobserved`/`OutcomeEnvelope` and the three drop-path tests; rename `"legacy-shell"` → `"shell"`. Preserve the `process.run`/`process.node` spans (asserted by trace tests and named in `docs/observability.md`) and `ProcessOutcome::TaskFailed` carrying the raw `JoinError`. It has been published once — yank or deprecate `0.11.x` pointing at `dekopon-run`.

**Decision: DEFER**

## [x] #17 — TEST_THEATER — crates/dekopond/src/tests.rs:375-668, :286-302, :248

**Finding:** 44 negative gateway-config cases — every endpoint-pinning case (`https://slack.evil.test`, `http://127.0.0.1@slack.evil.test`), credential-name, and file-permission case — assert only `is_err()` against a 30+-variant `ConfigError` with a dedicated `UnsupportedEndpoint`. A fixture typo that trips strict-decode passes forever without reaching the check it is named after; if `validate_endpoint` stops being called for a kind, the table stays green. The predicates are well tested in isolation; the *wiring* is not. (raw: C1)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Make the case tuple `(name, document, expected_variant)` and assert `matches!(load(..).expect_err(name), ConfigError::UnsupportedEndpoint { .. })` etc. per case; cases that legitimately expect `Decode` say so.

**Decision: APPROVE**

## [x] #18 — TEST_THEATER — crates/dekopon-provider-storage/src/lib.rs:362 (+ dekopon-provider-host/src/lib.rs:42)

**Finding:** `dekopon-provider-storage`'s only test is `STORAGE_WIT.starts_with("package dekopon:storage@0.1.0;")` on its vendored WIT copy — the exact check `provider-http/tests/wit_mirror.rs:3-6` documents as insufficient — while broker-host and provider-http verify byte-for-byte; `provider-host/src/lib.rs:42`'s `provider.wit` copy is verified by nothing. The storage contract is the one under active change (durable-files, JSONL, turso). (raw: C2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Add `crates/dekopon-provider-storage/tests/wit_mirror.rs` (`assert_eq!(STORAGE_WIT, include_str!("../../../wit/storage/storage.wit"))`, in `tests/` so packaged builds are unaffected) and the same one-liner for `provider-host` vs `provider-sdk/wit/provider.wit`; delete the surviving prefix check in `provider-http/src/lib.rs:355-359`; add a table test for `provider-storage`'s 11-variant `From<wit::StorageError>`.

**Decision: APPROVE**

## [x] #19 — TEST_SUITE_BLOAT — crates/*/tests/*.rs (no `tests/common/` anywhere)

**Finding:** ~900 LOC of copy-pasted test scaffolding across the workspace: `fn fixture(name)` ×9 (7 byte-identical), 13 hand-rolled HTTP/1.1 loopback parsers across 8 files (`content_length` byte-identical twice), 9 `tracing` capture layers with byte-identical `record_debug` (~400 LOC), 6 tree-snapshot walkers, 12 identical `#[allow(let_underscore_must_use, reason = "a dropped sender…")]` server-spawn blocks in `brokerd/tests/server.rs`. Copies have already drifted (`mock_http` truncates to content-length in broker-host, not in broker; `refusal_logging.rs`'s layer has the `register_callsite` override, `span_redaction.rs`'s does not). Also: `dekopon-model/src/image.rs:476` re-implements `src/mock.rs`'s `MockServer` minus its unwedge, and dekopond's Telegram tests have 11 inline constructors while Slack has a helper. (raw: C2, C1)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** One `crates/dekopon-test-support` dev-only crate (path dev-dep, not published) with `provider_fixture(name)` (panicking with the fetch-script name when missing), a `LoopbackServer` builder covering once/n/stalled/sequence/pooled, `content_length`, one `CaptureLayer` (with the `register_callsite` + `enabled` pair), `snapshot_tree`, and a `spawn_server` helper owning the one reasoned allow; then delete the copies. In-crate: delete `image.rs:476-551` in favour of `crate::mock`; add `telegram()`/`telegram_handler()` to `dekopond/src/tests.rs`. Decide the content-length truncation difference explicitly; keep the three non-identical allows in `server.rs` (lines 662, 966, 999) with their own text.

**Decision: APPROVE**

## [x] #20 — WRONG_ABSTRACTION — crates/dekopon-broker-protocol/src/lib.rs:1279 (BrokerRequest) + dekopon-broker/src/lib.rs entry points

**Finding:** Attestation shape is a type-level axis — direct / attested / chat-attested — multiplied across 11 `BrokerRequest` variants, 13 client methods, 11 server dispatch arms (four invoke arms repeating ~30 lines each), and 9 `Broker` entry points, while the only consumer (`dekopon-agent/src/lib.rs:241`) already models it as one `Option<Attestation { scope: Option<..> }>` and re-explodes it at every call. The matrix is ragged: there is no `ResolveCommandFor`, so `dekopon console`'s attested leg resolves command words through the unattested path. This multiplication is why #5 and #12 had to be written three times. (raw: A2)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** One operation per verb carrying `attestation: Option<Attestation>`, and one `Broker::resolve_context(peer, grant, attestation)` that the existing `resolve_attestation`/`resolve_chat_claim` become. Preserve the documented protocol version seam (version the envelope, or keep old tags as deprecated aliases for one cycle), `capabilities_for`'s opaque `Option` refusal, and `record` reachable only through its own operation. Do last — after #12 and #15 shrink the surface.

**Decision: APPROVE**

## [x] #21 — REINVENTED_WHEEL — crates/dekopon-brokerd/src/provider_manager.rs:694-1361 (+ :1983-2478)

**Finding:** ~670 lines hand-roll an OCI Distribution v2 client (reference grammar, Bearer-challenge parsing via a new `http-auth` dep, token cache, manifest/blob pull, descriptor structs) inside the privileged broker, tested only against an `axum` mock the same PR wrote; `oci-client` + `oci-spec` own all of it. The same file carries a 73-variant `ProviderManagerError` (495 lines, 20% of the file) that no consumer matches on — both live uses are `#[source]` in a thiserror wrapper. **Auditors disagree:** F says replace with a ~120-line wrapper preserving the controls; E rejected the finding because the controls (byte-bounded reads, loopback-only plaintext redirect policy, `no_proxy`/`http1_only`, exact-tag-or-digest only, no client at startup, registry URLs stripped from errors) are the point and `oci-client` does not expose them. (raw: F, E, B1)
**Blast radius:** HIGH  **Confidence:** MEDIUM
**Suggested fix:** Owner's call. If replacing: `oci_client::{Reference, Client, RegistryAuth}` + `oci_spec::image` types, keep `bounded_response_bytes`, `registry_redirect_policy`, `redirect_target_allowed`, `is_literal_loopback_registry`, and the selector rule as a wrapper, drop `http-auth`. Either way: collapse `ProviderManagerError` to ~10 variants a caller could branch on (`Configuration`, `FileSecurity`, `Registry`, `DigestMismatch`, `LockMismatch`, `StoreFull`, `Host`, `Io`) each with `reason: &'static str` + optional `#[source]`, preserving the exact refusal strings tests assert on and the no-secret-bytes-in-messages property.

**Decision: MODIFY: leave the hand-rolled OCI client and its controls alone. Only collapse `ProviderManagerError` to ~10 variants (`Configuration`, `FileSecurity`, `Registry`, `DigestMismatch`, `LockMismatch`, `StoreFull`, `Host`, `Io`), each with `reason: &'static str` + optional `#[source]`, preserving the exact refusal strings tests assert on and the no-secret-bytes-in-messages property.**

## [x] #22 — SIDE_QUEST — crates/dekopon-tui/ (whole crate) + crates/dekopon/src/console.rs

**Finding:** `dekopon console` (#164) added 6,170 lines and a TUI framework to the `kubectl`-shaped operator CLI, is unrunnable against the shipped chart (`allowDevelopmentSubjects` is set nowhere; default off), cost five permanent `deny.toml` duplicate-version exemptions (all from `ratatui`), added a `SubjectService::Dev` namespace to the trusted core crate, and is described in 13 documents — while `design.md:292`'s named operator commands (`apply`, `delete`, `logs`, `auth can-i`, `policy explain`) remain unbuilt. An interactive TUI was an explicit 0.1 non-goal in `roadmap.md`. (raw: B1, B2)
**Blast radius:** HIGH  **Confidence:** MEDIUM
**Suggested fix:** Move it out of tree the way `gh` and `turso-sql` went — a `dekopon-agents/dekopon-console` repo consuming `dekopon-agent` + `dekopon-broker-protocol` from crates.io — dropping `ratatui`/`crossterm` and the five `deny.toml` entries from the control plane. If kept: the render-time redaction and terminal-control sanitisation in `dekopon-tui/src/redact.rs` is a real security property and must travel with the code; and the six untested console flags (`--model --endpoint --api-key-env --auth-file --max-steps --max-capability-calls`) need `crates/dekopon/tests/cli.rs` cases.

**Decision: APPROVE**

## [x] #23 — WRONG_ABSTRACTION — crates/dekopon-shell/src/builtins/text/mod.rs:7-9

**Finding:** Seven text builtins (1,383 LOC, 42 tests) implement a literal-only pattern language justified by "a regex engine is a large dependency and a large attack surface" — but `Cargo.toml:57` enables `jaq-std`'s `regex` feature, `regex-bites` is linked into `dekopon-shell`, and `jq.rs:472`'s two-name denylist leaves `test`/`match`/`capture`/`sub`/`gsub`/`scan`/`splits` script-reachable today. The most common shell idiom a model writes (`grep "[0-9]"`, `sed "s/^ *//"`) gets a usage error telling it to use jq — the regex engine that is already there. (raw: A3)
**Blast radius:** HIGH  **Confidence:** MEDIUM
**Suggested fix:** Decide, then make the comment true. (a) Accept `-E` on `grep`/`sed` backed by the already-linked engine, keeping the literal default and by-name rejection for the unflagged case; or (b) keep literal-only and rewrite `text/mod.rs:7-9` to state the real reason (one matching semantics across `grep`/`sed`/`case`/`${p#…}`/`[[ == ]]`). Preserve the never-silently-mismatch invariant.

**Decision: MODIFY: option (a). Accept `-E` on `grep`/`sed` backed by the already-linked regex engine; keep the literal default and the by-name rejection for the unflagged case; rewrite `text/mod.rs:7-9` so the comment matches. Preserve the never-silently-mismatch invariant.**

## [x] #24 — FEATURE_FLAG_SPRAWL — crates/dekopon-{core,capability,protocol}/Cargo.toml:17 (schemars)

**Finding:** The `schemars` feature is default-on in three foundational crates, produces 40 `JsonSchema` derives, and the whole workspace has one call site — a test asserting `schema["title"] == "Agent"`. Every CI gate builds `--all-features`, so no gate ever compiles the three crates with it off; `dekopon-protocol` lacks `default-features = false`, so its feature re-enables the other two for anything with protocol in its graph, defeating the isolation `dekopon-core/Cargo.toml:18-19` describes. crates.io consumers pay `schemars` + `schemars_derive` + `syn` by default. (raw: B2)
**Blast radius:** HIGH  **Confidence:** MEDIUM
**Suggested fix:** Flip all three to `default = []` (consumers opt in), or delete the feature and the derives. If kept as a public promise, add a real golden-schema round-trip test and a `--no-default-features` build of the three crates to CI.

**Decision: MODIFY: flip to opt-in. `default = []` in dekopon-core, dekopon-capability, dekopon-protocol; `default-features = false` on protocol's edges to core/capability so the feature stays isolated; keep the feature and the 40 derives for consumers who ask. Add one CI build of the three crates without the feature so the off state stays compilable.**

## [x] #25 — SIDE_QUEST — crates/dekopon-webui/ (whole crate) + charts/dekopon/templates/_helpers.tpl:457-458

**Finding:** A 1,973-line crate reachable only through `--http-bind`, which the chart cannot pass — `args: ["--config", …]` is a literal with no values key reaching argv. Seven documents (including a full threat paragraph in `security-model.md:143`) describe an unauthenticated GET listener sharing the privileged broker's address space that no supported deployment can enable; `dekopond` carries the gateway-side inventory/model-usage reports that exist only to feed it. (raw: B1, B2)
**Blast radius:** MEDIUM  **Confidence:** HIGH
**Suggested fix:** Decide it one way. Either add `broker.httpBind: ""` to `values.yaml`, append `--http-bind` in `dekopon.brokerContainer` when set, and render one CI combination with it on (no Service/Ingress); or delete the crate, the flag, the `ReportAgentInventory`/model-usage protocol messages, and cut the seven doc sections to one line.

**Decision: MODIFY: keep the crate. Add `broker.httpBind: ""` to `charts/dekopon/values.yaml`, append `--http-bind <value>` in `dekopon.brokerContainer` when it is set, and render one CI chart combination with it on (no Service/Ingress). Keep the `ReportAgentInventory`/model-usage protocol messages and the docs.**

## [ ] #26 — SIDE_QUEST — crates/dekopond/src/transport/whatsapp.rs (1,704 LOC) + telegram.rs (693 LOC)

**Finding:** The live gateway runs Slack ×3 + Discord ×1. WhatsApp is the product's only inbound listener, only HMAC path, and three gateway-held credentials, with 15 documents and a chart `ClusterIP` service nobody uses; Telegram has no example directory at all (`telegramLongPoll` appears only in `docs/dekopond.md` and unit tests) yet threads `ChatScope::Telegram`/`SubjectService::Telegram` into the trusted broker/core crates. Both widen the chat-scope authorization surface every change must carry. (raw: B1, B2)
**Blast radius:** MEDIUM  **Confidence:** HIGH
**Suggested fix:** Decide both together: either `dekopond` grows a transport seam that lives out of tree, or delete both (`whatsapp.rs`, `telegram.rs`, `examples/whatsapp/`, the chart `gateway.service` block, the `ChatTransportKind`/`ChatScope` variants and broker congruence arms). Preserve: `SubjectService` must keep *parsing* `whatsapp.<id>`/`tel.<n>`/`telegram.<id>` subjects so existing audit chains stay readable.

**Decision: DEFER**

## [x] #27 — SIDE_QUEST / PREMATURE_GENERALIZATION — crates/dekopon-model/src/image.rs + crates/dekopond/src/config.rs:360 + session.rs image paths

**Finding:** Route-scoped image generation — a 552-LOC OpenAI Images client, a one-variant tagged `ImageGeneratorConfig` enum with four match-accessors, a name-keyed `HashMap<String, Arc<dyn ImageGenerator>>` over one impl, upload paths in three transports, `ChatReply::with_image`, five config error variants, and an authority paragraph in `design.md:113` — is configured in zero examples, chart values, or deployments (`imageGenerators` occurs only in `dekopond/src/tests.rs` and two docs). (raw: B1, A1, B2)
**Blast radius:** MEDIUM  **Confidence:** HIGH
**Suggested fix:** Delete it (`image.rs`, `ImageGeneratorConfig`, `RouteConfig::image_generator`, the three `upload_*`/`create_message_with_image` paths, `ChatReply::with_image`, the design/security-model paragraphs). Preserve the *inbound* asset path (`asset.rs`, `fetch_chat_asset`) and `Modality`/`accepts_images`, which are live for images handed *to* a model. If kept instead: collapse to a plain struct + `Option<Arc<OpenAiImageGenerator>>` and add an example route.

**Decision: MODIFY: keep it. Collapse `ImageGeneratorConfig` to a plain struct and the name-keyed registry to `Option<Arc<OpenAiImageGenerator>>`, drop the four match-accessors, and add an example route that configures it. Inbound asset path untouched.**

## [x] #28 — SIDE_QUEST / COMMENT_ROT — crates/dekopon-agent/src/prompt.rs:1457 vs :1461

**Finding:** Inside one `SCRIPT_TOOL_DESCRIPTION` string, line 1457 promises `set -e`/`[[ ]]` work and line 1461 tells the model they "are errors, never silent no-ops". Both are supported since #165 (`parser.rs:702`, `interp.rs:891`). No human writes these scripts — the prompt *is* the interpreter's API doc — so the two constructs #165 was built to provide are unreachable in practice. (raw: B1)
**Blast radius:** MEDIUM  **Confidence:** HIGH
**Suggested fix:** Remove `` `[[ ]]` `` and `` `set -e` `` from the exclusion list at :1461; add a test asserting every construct still named there produces `FatalError::Unsupported` (mirroring `builtins/mod.rs:395`'s registry-vs-docs assertion).

**Decision: APPROVE**

## [x] #29 — WRONG_ABSTRACTION / DUPLICATED_LOGIC — crates/dekopon-telemetry/src/lib.rs (owns exporters, not wiring)

**Finding:** `docs/design.md:103` says `dekopon-telemetry` owns "subscriber wiring"; it owns none. Four binaries hand-roll the same registry/`EnvFilter`/JSON-stdout/OTLP-layer/flush-shutdown sequence (`dekopond/src/main.rs:68-140` and `brokerd/src/main.rs:101-186` are the same function with different strings), each with its own `OTEL_TRACE_FILTER` const; `error_chain` exists at four sites with one divergent copy (`dekopond` skips the top-level `Display`, so the two daemons' exit records have different field shapes); the `RecordTargets` self-log regression test is copied verbatim three times; `dekopon/src/lib.rs:176` discards `try_init()`'s result into a `_named` binding with no log. (raw: A3, D2)
**Blast radius:** MEDIUM  **Confidence:** HIGH
**Suggested fix:** One `error_chain` in `dekopon-core` (the `error.to_string()` variant — three of four use it); a `dekopon_telemetry::install(...)` builder taking what varies (stdout-vs-stderr, json, crate filter directive, optional extra layer) and returning a guard whose `shutdown()` flushes both providers; one parameterized `RecordTargets` test. Preserve the four different failure policies (runner fails the command on flush failure; daemons log and continue; brokerd's stderr-only provider mode; the CLI's `io::sink`) and `dekopon-run`'s byte-exact stderr output.

**Decision: APPROVE**

## [x] #30 — VERSION_DRIFT_RISK — Cargo.toml:41 (fs2)

**Finding:** `fs2 0.4.3` — an abandoned pre-1.0 `unsafe`-libc wrapper — is linked in six files on the durability path of both privileged processes (audit writer lock, checkpoint lock, storage leases, provider-store lock) solely for advisory flock; `std::fs::File::{lock, try_lock, unlock}` stabilized in Rust 1.89.0, exactly this workspace's `rust-version`. The workspace already carries `rustix` and `libc` for the same syscall layer. (raw: E)
**Blast radius:** MEDIUM  **Confidence:** HIGH
**Suggested fix:** Drop `fs2`; use `File::{try_lock, lock, unlock}`. The gap is the contention error type at exactly three sites (`layout.rs:760`, `namespace.rs:898`, `provider_manager.rs:1562`): `fs2` gives `io::Error` with `ErrorKind::WouldBlock`, `std` gives `TryLockError::WouldBlock` — keep distinguishing contention from I/O failure, and move the `layout.rs:1157` fixture with them. Keep `namespace.rs:891`'s poll-with-deadline loop as is.

**Decision: APPROVE**

## [x] #31 — DUPLICATED_LOGIC — crates/dekopond/src/transport/{slack,discord,telegram,whatsapp}.rs

**Finding:** `transport.rs` holds real shared helpers, but per transport: backoff ×3 (identical formula/constants, jitter divergent — Slack/Telegram `pid % 250` once per process, Discord hashing the pid through `RandomState`, whose behaviour std does not guarantee; `getrandom` is already a dep), `Dedup` ×2 byte-identical + a third shape with a 4× different bound (1024 vs 4096), `retry_after` body parsing ×3 in Discord and absent from Slack/WhatsApp/Telegram's send path, the reconnect skeleton ×2, WhatsApp's `Service` code spelled `"429"` where the others use `"http-429"`, and a `split_message` fork in `whatsapp.rs:948` shadowing the shared one (deliberately scalar-counted, but its empty-input behaviour silently differs). (raw: D2, E, A1)
**Blast radius:** MEDIUM  **Confidence:** HIGH
**Suggested fix:** Lift into `transport.rs`: one `reconnect_delay(failures)` with `getrandom` jitter and one constant pair; one `SeenIds` ring (WhatsApp's claim/release as `ClaimedIds` on top, with its own named capacity if 4096 is deliberate); one `retry_after_from_body(&Value, max)`; a `split_message` parameterized on the counting unit. Normalize WhatsApp's code to `"http-{status}"`. Preserve Telegram's offset-based idempotency, Discord's resume-vs-identify handshake, the `failures.min(7)` clamp, and the server-directed waits (`retry_after`) as separate from jittered backoff. Do after #26 decides which transports remain.

**Decision: APPROVE**

---

*Entries #32–#35 are auditor D1's (DEAD_CODE / COMMENT_ROT), appended after the sort above; #32 and #33 are HIGH blast and belong in Wave 0/1.*

## [x] #32 — DEAD_CODE — crates/dekopon-tui/src/app.rs:308,320 + src/run.rs:51,130

**Finding:** Two of the ten keys the console's help overlay advertises — `o` (expand a capability call) and `r` (reveal a redacted field) — have no arm in `run.rs::on_key`, so `App::toggle_call`, `App::reveal`, `expanded_call`, `revealed`, the `▸` disclosure branch and the `[N redacted · r to reveal]` hint are all unreachable in the running console; only unit tests call them (which is what hid it). Same commit (#164) also enables mouse capture and discards every mouse event, taking native text selection/scroll away from the operator for nothing. `crates/dekopon-tui/README.md:91` and `docs/cli.md:171` both promise "revealing is one keystroke against one field". (raw: D1)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Either add the `o`/`r` arms with the per-call/per-field cursor they need (preserving per-field, per-keystroke, never-a-mode reveal semantics and the scrollback notice `App::reveal` emits), or delete `toggle_call`/`reveal`/`is_revealed`/`expanded_call`/`revealed`, the `expanded` render branch, the hint, the two help rows, and the two doc sentences. Drop `EnableMouseCapture`/`DisableMouseCapture` either way. If #22 moves the crate out of tree, this travels with it.

**Decision: MODIFY: do it as part of #22's extraction so the moved crate lands working: add the `o`/`r` arms with the per-call/per-field cursor they need (per-field, per-keystroke, never-a-mode reveal, keep the scrollback notice `App::reveal` emits) and drop `EnableMouseCapture`/`DisableMouseCapture`.**

## [x] #33 — COMMENT_ROT — README.md:5,136,311; docs/design.md:174; docs/security-model.md:299; docs/broker-http.md:11,60; docs/development.md:166; crates/dekopon/README.md:3 (+3 other stale facts)

**Finding:** "The operator CLI is not integrated with the broker" is asserted in nine places — including the README status blockquote and the threat model's own "what this project does not have" list — and `dekopon console` (#164) made it false; the same two documents describe the console as an attested broker client a few lines away. Three more stale facts: `docs/dekopond.md:462` cites a 180 s pod grace the chart now *refuses* to render (`terminationGracePeriodSeconds: 270`, and the chart README uses 180 as its rejected example); `docs/catalog.md:10` says "two reserved names" while `:192` says "four fields"; `.gitignore:21-23`'s rationale comment describes the walkthrough that left in #155 (covered by #1). (raw: D1)
**Blast radius:** HIGH  **Confidence:** HIGH
**Suggested fix:** Decide the term once: if "operator-CLI integration" stays a term of art for administrative subcommands, qualify the six term-of-art sites with "beyond `console`" and rewrite the two unqualified glosses (`README.md:5`, `:136`); `docs/security-model.md:299` must name the console explicitly; `crates/dekopon/README.md:3` should mention it at all. Change `dekopond.md:462` to 270 s, name `drainBudget`, link the chart README. Make `catalog.md:10` say "four".

**Decision: MODIFY: with #22 moving the console out, leave the nine "operator CLI is not integrated with the broker" sites as they are (true again) and make #22's move strip every console description from README/security-model/design/cli docs. Fix the three other facts: `docs/dekopond.md:462` → 270 s, name `drainBudget`, link the chart README; `docs/catalog.md:10` → "four"; `.gitignore:21-23` comment goes with #1.**

## [x] #34 — DEAD_CODE — 5 zero-reference `pub` items + 13 test-only `pub` items + the one `#[allow(dead_code)]`

**Finding:** Zero references anywhere: `dekopon-agent/src/prompt.rs:912 script_outcome_label` (orphaned by #52; its docstring names an attribute nothing exports), `dekopon-policy/src/lib.rs:211 provider_for`, `dekopon-provider-http/src/lib.rs:132 header_values`, `testkit/src/lib.rs:155 storage_evidence`, `:510 temporary_dir` — two landed after the #147 dead-surface sweep. Test-only consumers: `verify_audit_chain` (18 test uses, no operator path — the audit-chain integrity check is unreachable to an operator), `InMemoryAuditLog`, `PROVIDER_WIT`/`STORAGE_WIT`/`HTTP_WIT` declared in 2–3 crates each, `open_handle_count`, `is_bare_command_substitution`, `truthy`, `is_answered`, `from_turns`, `storage_root`. And `chatgpt.rs:2785 #[allow(dead_code)] fn _assert_private_path` — a no-op stub with a security-shaped name, added with the file. All are published API on crates.io crates. (raw: D1)
**Blast radius:** MEDIUM  **Confidence:** HIGH
**Suggested fix:** Delete the five zero-reference items (`header_values`: give it a use in `http-probe` or delete) and the `_assert_private_path` stub; demote `truthy`, `open_handle_count`, `is_bare_command_substitution`, `from_turns`, `storage_root` to `pub(crate)`; keep each `*_WIT` const only where a foreign crate reads it (#18 makes the mirror tests the reader). `verify_audit_chain` is a product call: give it `dekopon-brokerd audit verify --audit-path …` beside the existing `provider list|verify` (same shape at `main.rs:256-300`) or delete it — do not hide it behind `pub(crate)`. Leave `InMemoryAuditLog` (documented "for tests and embedding").

**Decision: APPROVE (CLI branch): add `dekopon-brokerd audit verify --audit-path …` beside `provider list|verify` and keep `verify_audit_chain` pub; everything else as listed.**

## [x] #35 — DEAD_CODE — crates/dekopon-provider-sdk-testkit/src/lib.rs:259,266,276,283

**Finding:** Four of `FakeBrokerBuilder`'s eleven methods are called by no test in the repo — `storage_limits`, `host_limits`, `compile_cache`, `continuity` — while the crate's README makes `.compile_cache(dir)` a headline recommendation and builds its strongest claim ("a quota a test trips here is a quota production would have tripped") on `storage_limits`. This crate is the public testing contract for out-of-tree provider authors, and six of its documented affordances have never executed once. (raw: D1)
**Blast radius:** MEDIUM  **Confidence:** HIGH
**Suggested fix:** One test per unexercised method in `tests/harness.rs`: `compile_cache` loading the same component twice into a `TempDir` and asserting the cache is non-empty; `storage_limits` tripping a tiny quota; `continuity(AuthorityBound)` asserting a fresh generation; `host_limits` tripping a fuel or memory bound. Preserve the builder's `Stable` default.

**Decision: APPROVE**
