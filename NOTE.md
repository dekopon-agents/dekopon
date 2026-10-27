# Model CA approach checkpoint

Steering: "# Steering for GPT executors" (xrl/agents cc070358504191cc739c6aaecf124e91da8682aa).
Base: 999c6c05a785c7beca52903883aece2b87f8f0a6 (v0.59.0).
Branch: codex/model-ca-1. No implementation before coordinator review.

## Conflicts and readings

- Acceptance 2 literally includes "empty by default" among refused cases. Outcome and contracts 1/3 require an accepted empty default. Accept absent/empty lists; refuse empty files.
- Brokerd validates CA settings fail-fast as InvalidHttpsConfiguration; gateway aggregates ConfigProblem values. Preserve gateway's typed aggregate style, including simultaneous conflicts, while mirroring brokerd limits and check-versus-boot behavior.
- Brokerd has Boot/Reload/Check; gateway LoadMode has only Boot/Check and load_directory currently ignores mode. Thread the existing mode through resolution; reload must reread via boot resolution, without adding a mode or config key.
- The existing model TLS test checks UnknownIssuer via a diagnostic string. New acceptance coverage will assert InferenceError::Transport(TransportFailure::Http) and downcast its source chain to rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer), never match Display.
- NOTE.md is required on the pushed branch but excluded from the final PR diff. Commit it now, then remove it in a subsequent commit before opening the PR; never amend a pushed commit. This checkpoint artifact removal is explicitly required, not a product deletion.
- Pinned steering prohibits new public APIs; AGENTS treats public crate APIs as internals. Extend existing constructors with a certificate slice and update callers, rather than introduce a parallel public constructor or setter. No unrelated caller migration.
- Repository guidance says branch from current main; the brief pins an immutable base. Use the brief's exact base.
- Steering requests a full final diff/report; the brief caps the report at 300 words. Follow the brief's cap and put detailed evidence in the PR body.
- No generic gateway extra-volume/mount mechanism exists in charts/dekopon/values.yaml. Document an operator-supplied read-only volume mount in the rendered deployment, adding no chart value or chart edit.

## Shape and files

- Add only top-level gateway extraCABundles: Vec<PathBuf>, serde-default empty, explicit spelling, deny_unknown_fields preserved. One list per gateway covers every InferenceHttp model transport.
- crates/dekopon-gatewayd/src/config.rs: validate at most eight absolute paths; check does no CA file I/O. Boot/reload require regular files of 1..=65,536 bytes and valid nonempty reqwest PEM certificate bundles. Follow brokerd's metadata/read/parse shape, preserving safe cause categories without certificate bytes in errors. Carry parsed certificates into resolved model construction without Debug-printable PEM storage.
- Mirror the small validator locally, with equality-pinning coverage of brokerd bounds, rather than introduce a broker dependency or shared public helper that expands the trust boundary.
- crates/dekopon-model/src/http.rs: accept parsed extra certificates and add each through reqwest add_root_certificate alongside default WebPKI roots. Preserve use_rustls_tls, no_proxy, no redirects/retries and all existing timeouts. Never consult environment or native trust stores.
- crates/dekopon-model/src/{openrouter,openai,codex}.rs: extend existing constructors/internal configured path to pass certificates to InferenceHttp. Update compile-required callers and tests across the workspace; empty slices preserve current behavior. No credential flow changes; Codex continues sharing CredentialFile owners. Codex OAuth refresh is outside InferenceHttp and remains unchanged.
- crates/dekopon-gatewayd/src/session.rs and configuration/reload consumers: carry gateway trust into all three clients. Ensure cached clients from an earlier configuration cannot be reused when bundles change, including same-path new contents. Trace/check output must never expose bytes.
- crates/dekopon-gatewayd/src/probe.rs: honor the same trust for explicit network probes while keeping plain check path-only; inspect probe lifecycle before implementing file reads for explicit probe.
- Companion test locations beside gateway config and model HTTP tests; update required constructor callers only. Existing rcgen, rustls and tokio-rustls test dependencies suffice for model TLS coverage; reuse adjacent harnesses before adding setup.
- docs/gatewayd.md and docs/inference.md: key placement, default, limits, boot/reload/check semantics, additive trust and explicit read-only mount instructions. State that a listed CA's key holder can impersonate model endpoints to this gateway.
- docs/security-model.md: distinguish broker http.extraCABundles from gateway extraCABundles; retain exclusions for OTLP and OCI provider manager.
- docs/upgrading.md: clarify separation from OTLP's existing CA setting if needed.
- No chart, WIT, wire format, CHANGELOG.md, credential, tag, release or deployed configuration changes.

## Tests first

1. Generate CA and CA-signed localhost leaf in test, run a bounded local HTTPS peer and join its task. Same request succeeds with the extra CA and fails without it, asserting typed Http and UnknownIssuer variants. No certificate assets committed.
2. Config default absent versus explicit [] equivalence, strict unknown fields, relative path, missing/empty/over-limit/non-PEM files, >8 paths; assert typed problems. Include simultaneous validation conflicts.
3. Check refuses relative paths and accepts absolute nonexistent paths without reading them, in both single-file and fragment loading.
4. Boot/reload read and reject changed invalid bundle contents; verify new generation clients receive changed trust rather than cached old clients.
5. Exercise all three InferenceHttp constructor paths and explicit probe trust forwarding, using neighboring test helpers; no transport exclusion among OpenRouter, OpenAI-compatible and Codex inference.
6. Existing absent-key request fixtures remain byte-identical; no serialized TLS data or new headers.

## Gates

Before implementation builds, inspect disk and documented artifact lifecycle.
- git diff --check
- cargo fmt --all --check
- Focused model and gateway config/session/probe tests with --locked, then applicable workspace suites and doctests after fetching required external fixtures.
- cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
- cargo check --release --locked -p dekopon-brokerd -p dekopon-gatewayd -p dekopon-console
- RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
- cargo deny --all-features check
- cargo machete and both daemon dependency gates, including their Python gate tests.
- python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md
- Exercise gateway check executable against an applicable config; name every unavailable gate and exact reason in PR validation table. Run packaging only if dependency/package metadata changes require it.
- Inspect all comments against AGENTS, all source/error telemetry handling for byte disclosure, and final diff boundaries. Ready PR only after applicable gates pass and remote CI green. Coordinator alone launches fresh Astra review over immutable base/head SHAs.

## Release and evidence handoff

Deploy gateway binary supporting the key before introducing it into any config; v0.59.0 denies this unknown field. Broker, providers, WIT and chart consumers are unchanged. The brief identifies the RPi at v0.59.0 and requires its config to remain without the key; no live artifact inspection/deploy is authorized here. Coordinator verifies pinned artifacts and registers the first-release cloud eval check.

At the first release carrying this PR, coordinator runs the five 0049 evals with the cloud CA listed and records one successful OpenRouter model turn through the pinned gateway trace without TLS error. RPi remains unconfigured. This evidence is deferred, never claimed by local tests.

## Choices I made

- Use one top-level extraCABundles list to avoid repeated per-model trust configuration.
- Preserve gateway typed aggregate validation and mirror brokerd's compact local validation with bound parity coverage.
- Extend existing constructor signatures rather than add public parallel entry points; caller churn may exceed the approximate 250-line target and will be counted.
- Match rustls UnknownIssuer through typed source downcasting; retain the existing transport error shape.
- Treat trust contents as configuration-generation state so reload cannot retain stale pooled client trust.
- Exclude Codex OAuth refresh, OTLP, OCI manager and guest proxy forwarding clients because the contract scopes this addition to InferenceHttp.
- Publish the note as branch history and remove it before final PR, as the checkpoint requires.
