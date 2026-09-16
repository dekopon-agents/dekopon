# Guidance for coding agents

Dekopon runs self-hosted AI agents through Wasm providers.
The model proposes; a separate broker authorizes and executes provider effects.

## Read for the task

- Start with the [constitution](docs/design.md#constitution), including its non-goals.
  For behavior or architecture changes, read the relevant design sections.
- Use the [repository map](docs/development.md#repository-map) to find source and tests,
  then the applicable [change map](docs/development.md#change-maps).
- Read relevant [security model](docs/security-model.md) sections before changing identity,
  capabilities, credentials, providers, audit, or external effects.
- Select other contracts from the [area index](docs/README.md#change-a-specific-area);
  do not read every document by default or copy its inventory here.
- Follow [CONTRIBUTING.md](CONTRIBUTING.md#change-guidelines) for implementation and review conventions.

## Boundaries that must survive

- Only the broker grants provider authority; capability declarations permit proposals.
  Read authority never grants writes. External writes require explicit narrow capabilities.
- Identity comes from authenticated transport, never model, repository, or payload text.
  Instructions and skills are untrusted model text and grant no authority.
- Keep `dekopond` and `dekopon-brokerd` separate processes with separate UIDs.
  The gateway gains no policy, provider credentials, or authorization path;
  the broker gains no model orchestration. Preserve both dependency-boundary gates.
- Provider secrets stay broker-side, outside prompts, gateway/protocol, provider memory,
  evidence and logs. Model credentials stay in the selected model client.
  Configuration references credentials, never embeds values. Use `dekopon_core::Redacted`;
  minimize `expose`/`into_inner` sites. Never commit credentials or fetched provider fixtures.
- Preserve one complete W3C trace per message, including prompts, commands and effects;
  exclude secret bytes and daemon credentials. Bound attribute size, never span count.
- Distinguish Current, Committed direction and Exploration; code/tests prove what exists.
  Stop for human decision if the requested change conflicts with design or security.
- Publishing, releases, pushing/moving tags, adding credentials and protection changes require
  explicit human authorization for that action; release authorization names one version.
  Follow the [release procedure](README.md#maintainer-release-process), not memory.

## Change and verify

- Confirm repository root, branch and status; preserve unrelated work and artifacts.
  Start follow-ups from current main, not an already-merged feature branch.
- Follow the change map for companion tests, documentation, examples and changelog.
- Keep [WIT mirrors](docs/development.md#provider-contract-or-host) byte-identical;
  bump affected published contracts. Rebuild generated Wasm with pinned build scripts;
  regenerate Cargo locks with Cargo, never hand-edit them.
- Start with `git diff --check`, then the applicable [validation](docs/development.md#validation).
  Use `--locked`; root Cargo commands do not cover separate provider workspaces.
  Markdown-only work uses [documentation gates](docs/development.md#documentation-gates), not Rust builds.
- Check disk before expensive builds; follow the documented artifact lifecycle.
  Never delete active builds or another owner's artifacts.
- Report checks actually observed, exact head/artifact tested and verification gaps.
  Local tests do not prove deployed behavior or remote CI; never claim otherwise.
- Follow the [PR checklist](docs/development.md#before-opening-a-pull-request); required CI and human review precede merge.
  Automated agents never approve their own changes.
