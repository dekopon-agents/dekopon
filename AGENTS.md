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

## Rust guidelines

How dekopon's Rust is written, and what an implementing agent decides on its own versus asks
about. Where this section and the surrounding code disagree, match the code and say so in the
PR.

### Authority: what you decide and what you ask

- **Contract surfaces — stop and ask:** WIT, wire frames and their fields, config keys and
  values, chart values and mounts, what is deleted, and the proof-gate invariants (G1–G4).
- **Everything inside a crate is yours:** type and variant names, error enums, module layout,
  facade shapes, helper names, buffer sizes under a stated cap, test placement. Decide, mirror the
  nearest sibling, list the choice in your report. The verifier checks it; the owner does not
  pre-approve it. (Public crate APIs will be treated as contracts later; today they are yours.)

### Errors

- One `thiserror` enum per module or crate; variants carry `#[source]`; the sentence says what
  failed, not the value that failed. Never `String` errors, never `anyhow` in library code.
- Across a trust boundary carry `io::ErrorKind`, not `io::Error` (as `BlobError` does).
- Refusals are matched by variant in tests and callers, never by message text.

### Types and dispatch

- Variation by kind is a **closed enum matched exhaustively** (`Source::File`, later
  `Source::Channel`). No `Box<dyn Trait>` until a second implementer lives outside the crate.
- **Newtypes** for ids, byte counts, indexes and paths that mean different things
  (`AssetId(u64)`, not `u64`): the descriptor-index-versus-asset-id swap must not compile.
- Invariants are types or `Result`s. No `unwrap`, `expect` or slice indexing in non-test code
  (`clippy::unwrap_used`, `expect_used` denied); a truly impossible state is `unreachable!()`
  with a sentence.

### Bytes and memory

- Bytes **stream**: `std::io::Read`/`Write`, tokio `AsyncRead`/`AsyncWrite`, `base64`'s
  `EncoderWriter`/`DecoderReader`, `http_body::Body` with an exact `size_hint`. `Vec<u8>` only
  where the guest boundary forces it, and then ≤ one bounded chunk.
- Every growable buffer has a **named ceiling constant**; `Vec::with_capacity(cap)` once and
  reuse, not grow. A `.clone()` needs a comment saying why.
- Borrow first: `&str`, `&[u8]`, `&Path` parameters; own only what you store.
- `Arc` for shared ownership, never `Rc`; `Mutex` over `RwLock` unless measured; a lock scope
  holds no `await`.

### Async

- tokio only. Blocking file I/O goes through `spawn_blocking`, as `hydration.rs` does.
- `async fn` in traits is fine (the `bindgen!` host traits already are); spell
  `-> impl Future<Output = T> + Send` only where the future crosses a `spawn`. Never
  `async_trait`; no `Pin<Box<dyn Future>>` unless the future is stored in a struct.

### Dependencies

- Prefer what the workspace has: tokio, rustix, reqwest/http_body, base64, serde, thiserror,
  tracing. Wrap them; never re-implement (no hand-rolled base64, no custom framing when
  `http_body` exists). A new crate needs one sentence in the PR body and a tier-1 maintainer.

### Tests

- Names are sentences stating the invariant, like `a_queue_reads_one_image_at_a_time`.
- Real primitives: `UnixStream::pair()`, real files in a `tempdir`, loopback HTTP. Mock only the
  network. No trait-mocking frameworks.
- Every limit has a test at the boundary and one past it, asserting the variant.

### Comments

- Doc comments say **why** and the invariant, not what; match the density of `DiskBlob`'s.
- No narrating comments (`// step 1`, `// handle error`), no `TODO`/`FIXME` in a PR.

### The tells you are writing Python in Rust

`Option<String>` where an enum belongs; `Vec<u8>` handed whole between layers; `.clone()` to
appease the borrow checker; `HashMap<String, serde_json::Value>` as a struct; `unwrap()` on a
socket; a `bool` parameter; strings compared to select behaviour; a `Result<(), String>`.
