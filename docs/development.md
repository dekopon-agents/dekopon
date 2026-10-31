# Development

[`ci.yml`](../.github/workflows/ci.yml) is the gate list; run its steps for what you touched. This
page holds what CI and the code don't tell you.

## Validation

Start with `git diff --check` and pass `--locked` to Cargo. The shell's differential tests need GNU
Bash on `PATH`, which macOS lacks. As root, two permission tests cannot set up their failure
(`a_secret_file_that_cannot_be_opened_still_names_its_errno` and
`a_rotated_credential_completes_the_turn_when_the_write_fails`); report them as unverified.

### Documentation gates

The toolchain-free `documentation checks` job, required through `quality (stable)`, fails when
tracked Markdown outside `CHANGELOG.md`, `AGENTS.md` and the paths `ci.yml` excludes exceeds its
word budget, or `docs/` holds more than seven Markdown files. A Markdown-only change needs no Rust build.

## Provider contract or host

Each probe under `examples/providers/` is its own Cargo workspace. Root commands don't reach it, nor
does the root `[patch]` of `tracing-core`. Never edit a checked `.wasm`: rebuild it with its
`build.sh`, pinned by `rust-toolchain.toml` and `ci/toolchain.env`;
[`wit-package.yml`](../.github/workflows/wit-package.yml) rebuilds each one and fails on a changed
byte. WIT mirrors stay byte-identical (the `wit_mirror` and `host` tests list them), and a changed
contract takes a new package version.

The `gh`, JSONPlaceholder and memory-chat providers live in their own repositories;
`ci/fetch-external-provider-components.sh examples/providers` installs the pinned releases core
tests read. After a fixture bump a fresh worktree holds stale copies: refetch before bisecting.

### Memory-optimized Wasmtime

The Raspberry Pi runs an ARM64 build against a [Wasmtime fork](../.cargo/wasmtime-optimization.toml)
that shrinks compiled-code memory. Cargo features cannot select a `[patch]`, so the build loads its
own config and lock in a separate worktree:

```console
git worktree add --detach ../dekopon-wasmtime-optimization HEAD
cd ../dekopon-wasmtime-optimization
cp .cargo/wasmtime-optimization.lock Cargo.lock
cargo --config .cargo/wasmtime-optimization.toml build --release --locked \
  -p dekopon-brokerd -p dekopon-gatewayd -p dekopon-console
```

Never copy that lock into a release checkout. Moving the pin means regenerating it there with Cargo;
[`wasmtime-optimization.yml`](../.github/workflows/wasmtime-optimization.yml) tests both
architectures, as a non-required check.

## Releases

Feature pull requests never edit `CHANGELOG.md`. Each user-visible change is one unwrapped commit-message
line, `Changelog: <Added|Changed|Deprecated|Removed|Fixed|Security>: <text>`.

1. A prep pull request moves the workspace version (Cargo regenerates `Cargo.lock`, the optimized
   lock and the probe locks; rebuild the probes), the chart's `version` and `appVersion`, and writes
   dated `CHANGELOG.md` sections from `git log <previous-tag>..HEAD --format=%B | grep '^Changelog:
   '`.
2. After it merges, dispatch **Cut release** with `v<version>` and, if the chart changed,
   `dekopon-chart-<version>`, dry run first. It pushes the tags as the release GitHub App.
3. `release.yml` publishes the archives, images, tap formula and every crate in
   `.github/release-crates.txt`. If crate publication stops, dispatch **Release** with the tag and
   `publish_to_crates`; it skips published versions.

Each public crate needs a crates.io trusted publisher: `dekopon-agents/dekopon`, `release.yml`,
environment `crates-io`. A new crate name cannot trust-publish its first version: publish once with
an explicitly authorized scoped token, register the publisher, revoke the token. Tags and published
versions never move.

### Homebrew tap GitHub App

`GITHUB_TOKEN` cannot push to `dekopon-agents/homebrew-tap`, so the tap job mints a token from an
organization App. A maintainer sets it up once:

1. Create the App with its webhook off and only **Contents: Read and write**.
2. Generate a private key; GitHub offers the `.pem` once.
3. Install the App on `homebrew-tap` only.
4. Add repository secrets `TAP_APP_ID` and `TAP_APP_PRIVATE_KEY`, the whole `.pem`.

Until then the tap job warns and skips; the release still publishes. The App's token expires within
the hour, reaches one repository, and belongs to the organization, not a maintainer.

## Before opening a pull request

Branch from current `main`, fill in the [template](../.github/pull_request_template.md) with the
validation you ran, and use a conventional commit subject. Check required checks on the exact head
you pushed.
