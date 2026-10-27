# Approach: console image default

Steering: "# Steering for GPT executors" (cc070358504191cc739c6aaecf124e91da8682aa).
Base: 999c6c05a785c7beca52903883aece2b87f8f0a6. Branch: codex/console-image-1.

## Conflicts and readings

- The current helper requires a console tag/digest; contract 2 explicitly replaces that requirement with the main-image fallback.
- Contract 3's repository-only case currently fails rendering and conflicts with contract 2's unconditional empty-tag/digest fallback. Follow contract 2; byte identity covers overrides supplying their repository plus tag or digest.
- Contract 3 also conflicts with contract 1 for tag-only and digest-only overrides: they currently use the retired default repository. Follow contract 1: these partial overrides use ghcr.io/dekopon-agents/dekopon; byte identity covers overrides supplying their repository plus tag or digest.
- The chart README has no values table at this base. Update its existing image-default paragraph rather than inventing a table; update only Operator console and the 0.52.0 upgrade note elsewhere.
- The cited README Two version numbers section lives in docs/kubernetes.md. The coordinator overrides the brief’s version-bump instruction: leave Chart.yaml version at 0.46.0 and appVersion unchanged in this PR; the next chart release prep owns the version bump and matching CHANGELOG heading. This also keeps rendered chart labels unchanged for byte comparisons.
- The owner mentions a homelab PR, but this lane authorizes one Dekopon PR. Coordinator owns the homelab override removal.
- The requested campaign-trail/tools/cloud-env/HELPER-RULES.md is absent from the available workspace and pinned agents tree. Follow the brief's supplied rules: generic User-Agent, no personal request identifiers, PID-only process termination, remove any credential files written. No credentials will be read or written.
- Steering requests an unrestricted full final report; the brief caps the report at 300 words. Follow the brief's cap.

## Files and shape

- charts/dekopon/values.yaml: default console repository to ghcr.io/dekopon-agents/dekopon.
- charts/dekopon/templates/_helpers.tpl: preserve console digest then tag precedence; otherwise include dekopon.image using root context.
- charts/dekopon/README.md: describe inheritance; leave charts/dekopon/Chart.yaml unchanged per the coordinator ruling.
- docs/kubernetes.md Operator console and docs/upgrading.md 0.52.0: default matches daemon image, including variant/digest; explicit console references still override.
- .github/workflows/ci.yml existing chart render/assertion step: add assertions using its existing Helm/yq mechanism, with no new harness.

## Tests first

Extend the existing chart render step before the helper change: default appVersion, explicit main tag (including memory-optimized suffix), main digest, tag-only console override, digest-only console override, explicit custom console repository/tag and repository/digest, and digest-over-tag precedence. Assert the full image for tag-only overrides is ghcr.io/dekopon-agents/dekopon:<console-tag> and for digest-only overrides is ghcr.io/dekopon-agents/dekopon@<console-digest>; neither partial override inherits the main tag/digest. Compare console and broker image fields, not merely render success. Each fallback assertion fails against the base helper's required-tag error; explicit overrides prevent fallback from swallowing overrides.
Capture base renders for console disabled and the old explicit repository/digest; compare candidate bytes directly, with Chart.yaml unchanged. Use a synthetic digest if the homelab's exact values are unavailable; report that limitation, never claim live homelab evidence.

## Gates

Start with git diff --check. Run actionlint .github/workflows/*.yml for the planned workflow edit. Run the chart CI steps: shellcheck, Helm lint, all existing Python template tests, existing render/refusal assertions, Kubernetes schema validation, packaging/source-byte comparison, and init-permission checks when prerequisites exist. Run docs duplicate and audit-event checks. Run cargo fmt --all --check, workspace clippy --all-targets --all-features --locked -D warnings, workspace tests --all-features --locked, rustdoc --all-features --no-deps --locked with -D warnings, and cargo deny --all-features check as requested; name unavailable gates. Check disk before expensive builds. Helm and yq are currently unavailable and need installation before template tests.
Run CI’s Verify chart changelog against the unchanged chart version 0.46.0, whose heading already exists. Do not edit CHANGELOG.md; the next chart release prep supplies both the version bump and its heading, per coordinator ruling.

## Delivery after coordinator approval

No implementation before this checkpoint review. Remove NOTE.md in a later commit so it is absent from the final PR diff; never amend a pushed commit. One PR titled chart: console image defaults to the Dekopon image. Include counting-merge note, validation table, verbatim trust surface, coordinator-owned post-deploy kubectl image check, and every choice under Choices I made. Coordinator launches the independent Astra review. No merge, tag, release, deploy, main push, or live check here.

The image stop condition is not present: Dockerfile copies dekopon-console into /usr/local/bin; container-image.yml:249 checks its release-byte identity for both architectures, the shared variant path selects stock or wasmtime-optimization, and line 283 invokes it by name as UID 65535.

## Choices I made

- Follow contract 1 over contract 3 for tag-only and digest-only overrides: they use the new default repository; byte identity covers overrides supplying their repository plus tag or digest.
- Follow the coordinator ruling over the brief’s chart-version bump instruction: keep Chart.yaml at 0.46.0 in this PR and defer the bump and CHANGELOG heading to the next chart release prep.
- Follow contract 2 for repository-only overrides with empty console tag/digest: use the main image.
- Update the existing chart README image-default paragraph because this base has no values table.
- Leave the homelab PR and post-deploy live check to the coordinator under this lane’s one-PR scope.
- Follow the brief’s 300-word final-report cap over the steering’s full-report requirement.
