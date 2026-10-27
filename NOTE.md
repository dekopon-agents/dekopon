# Approach: console image default

Steering: "# Steering for GPT executors" (cc070358504191cc739c6aaecf124e91da8682aa).
Base: 999c6c05a785c7beca52903883aece2b87f8f0a6. Branch: codex/console-image-1.

## Conflicts and readings

- The current helper requires a console tag/digest; contract 2 explicitly replaces that requirement with the main-image fallback.
- Contract 3's repository-only case currently fails rendering and conflicts with contract 2's unconditional empty-tag/digest fallback. Follow contract 2; preserve byte-identical explicit console tag/digest references, including custom repositories.
- The chart README has no values table at this base. Update its existing image-default paragraph rather than inventing a table; update only Operator console and the 0.52.0 upgrade note elsewhere.
- The cited README Two version numbers section lives in docs/kubernetes.md. Follow that rule: chart patch 0.46.0 -> 0.46.1, appVersion unchanged.
- A chart version bump necessarily changes helm.sh/chart labels, conflicting with literal full-render byte identity. Compare baseline/candidate with chart metadata held constant for disabled-console and explicit old repository/digest cases; separately validate the final chart.
- The owner mentions a homelab PR, but this lane authorizes one Dekopon PR. Coordinator owns the homelab override removal.
- The requested campaign-trail/tools/cloud-env/HELPER-RULES.md is absent from the available workspace and pinned agents tree. Follow the brief's supplied rules: generic User-Agent, no personal request identifiers, PID-only process termination, remove any credential files written. No credentials will be read or written.
- Steering requests an unrestricted full final report; the brief caps the report at 300 words. Follow the brief's cap.

## Files and shape

- charts/dekopon/values.yaml: default console repository to ghcr.io/dekopon-agents/dekopon.
- charts/dekopon/templates/_helpers.tpl: preserve console digest then tag precedence; otherwise include dekopon.image using root context.
- charts/dekopon/Chart.yaml and charts/dekopon/README.md: patch chart version only and describe inheritance.
- docs/kubernetes.md Operator console and docs/upgrading.md 0.52.0: default matches daemon image, including variant/digest; explicit console references still override.
- .github/workflows/ci.yml existing chart render/assertion step: add assertions using its existing Helm/yq mechanism, with no new harness.

## Tests first

Extend the existing chart render step before the helper change: default appVersion, explicit main tag (including memory-optimized suffix), main digest, explicit console tag, explicit custom console repository/digest and digest-over-tag precedence. Compare console and broker image fields, not merely render success. Each fallback assertion fails against the base helper's required-tag error; explicit overrides prevent fallback from swallowing overrides.
Capture base renders for console disabled and the old explicit repository/digest; compare candidate bytes with Chart.yaml metadata held at the base version. Use a synthetic digest if the homelab's exact values are unavailable; report that limitation, never claim live homelab evidence.

## Gates

Start with git diff --check. Run the chart CI steps: shellcheck, Helm lint, all existing Python template tests, existing render/refusal assertions, Kubernetes schema validation, packaging/source-byte comparison, and init-permission checks when prerequisites exist. Run docs duplicate and audit-event checks. Run cargo fmt --all --check, workspace clippy --all-targets --all-features --locked -D warnings, workspace tests --all-features --locked, rustdoc --all-features --no-deps --locked with -D warnings, and cargo deny --all-features check as requested; name unavailable gates. Check disk before expensive builds. Helm and yq are currently unavailable and need installation before template tests.
Chart changelog validation requires a release heading for the bumped version, while PRs must not edit CHANGELOG.md: record any resulting release-prep dependency instead of altering the changelog.

## Delivery after coordinator approval

No implementation before this checkpoint review. Remove NOTE.md in a later commit so it is absent from the final PR diff; never amend a pushed commit. One PR titled chart: console image defaults to the Dekopon image. Include counting-merge note, validation table, verbatim trust surface, coordinator-owned post-deploy kubectl image check, and every choice under Choices I made. Coordinator launches the independent Astra review. No merge, tag, release, deploy, main push, or live check here.

The image stop condition is not present: Dockerfile copies dekopon-console into /usr/local/bin; container-image.yml:249 checks its release-byte identity for both architectures, the shared variant path selects stock or wasmtime-optimization, and line 283 invokes it by name as UID 65535.
