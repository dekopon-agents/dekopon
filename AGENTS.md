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
- This file and the documents it links are the whole contract. A cloud agent in a bare checkout has
  no owner memories, machine instructions or sibling repositories, and needs none: fetch what a
  gate needs (`ci/fetch-external-provider-components.sh`), and name every gate you could not run.

## Boundaries that must survive

- Only the broker grants provider authority; capability declarations permit proposals.
  Read authority never grants writes. External writes require explicit narrow capabilities.
- Identity comes from authenticated transport, never model, repository, or payload text.
  Instructions and skills are untrusted model text and grant no authority.
- Keep `dekopon-gatewayd` and `dekopon-brokerd` separate processes with separate UIDs.
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
- Human intervention is not required when the plan is sufficiently scoped out and the changes
  are expected. Scoped out: the owner approved a plan that names the change, its version and its
  rollout. Expected: the diff does what the plan said and nothing else. Inside that, merge, tag,
  release and deploy without waiting. A milestone the owner has funded in his campaign file is
  explicit human authorization for that milestone's named version, its merges and its rollout.
  The fresh adversarial review of the merged head is the review.
- Stop for the owner when the work leaves the plan: a change it did not name, a result that
  surprises its reviewer, a version it did not list, a new credential, a protection change.

## Committed direction

Decided by the owner on 2026-09-29. Code follows in the 0.28 to 0.33 releases. Design new work
against these; a change that moves away from one stops for a human decision.

- **No call-site rules.** Anything that can run, runs from any depth: a model's script, a
  provider, a nested provider, a job. Bounds are memory, time, output and the turn's call budget,
  shared by the whole tree. Authority is not a call site: the broker authorizes every invocation
  for the person and agent, and a probe stays read-only.
- **Streams only.** A provider's output is its stdout. No contract offers a returned value beside
  a stream, a buffered call beside a streaming one, or a shim for the previous epoch.
- **Serving beats recording.** Telemetry is best effort: a full queue drops and counts, and
  nothing in a serving path waits on an exporter. One complete trace per message is the aim,
  never a reason to stall a reply.

## Proportionate remedies

Audits and plans default to over-building, and the owner reels them back. Before proposing a remedy:

- **Delete before adding.** Check whether an existing owner already makes the hazard impossible
  (a session gate, `&mut self`, a lock that already serializes the effect); if so, remove the
  redundant primitive instead of adding coordination.
- **Enforcement is compiler-native only:** `clippy.toml` `disallowed-*` with a `reason`, a site
  `#[expect(..., reason)]`, workspace lints. No CI checker scripts, grep gates, exception
  registries, negative-fixture suites or per-boundary inventory docs.
- **Fail fast, one rich trace record, a reset path.** No quarantine, integrity MACs or detection
  that cannot fix anything ([non-goals](docs/design.md#non-goals)).
- **Calibrate severity to the one deployment,** a Raspberry Pi serving family chats. Owner-only
  and dev-only paths are P3; a process-killing input is P1 whatever the path.
- **Compatibility is a constraint only when the owner set it.** Everything is pre-1.0; breakage in
  core and providers is expected. Before designing around a break, ask what breaking it would delete.
- **Fix it where it runs first.** A defect found in the chart is fixed in the deployment's values
  first; the chart fix rides the next release.
- **Recommend; don't enumerate.** Bring one proposal plus the few decisions only the owner can make.

## Change and verify

- An opt-in [memory-optimized Wasmtime build](docs/development.md#memory-optimized-wasmtime) is available; stock Wasmtime remains the default.
- Confirm repository root, branch and status; preserve unrelated work and artifacts.
  Start follow-ups from current main, not an already-merged feature branch.
- Follow the change map for companion tests, documentation and examples. Never edit `CHANGELOG.md` in a PR; write `Changelog: <Category>: <text>` lines in commit messages instead (`docs/development.md`).
- A change to a CI gate edits the sentence that describes it. An example config the change touches
  still starts, or is deleted.
- Add no comments or doc comments unless they meet [Comments](#comments); delete the ones your
  change makes stale instead of rewording them.
- Keep [WIT mirrors](docs/development.md#provider-contract-or-host) byte-identical;
  bump affected published contracts. Rebuild generated Wasm with pinned build scripts;
  regenerate Cargo locks with Cargo, never hand-edit them.
- Start with `git diff --check`, then the applicable [validation](docs/development.md#validation).
  Use `--locked`; root Cargo commands do not cover separate provider workspaces.
  Markdown-only work uses [documentation gates](docs/development.md#documentation-gates), not Rust builds.
- Check disk before expensive builds; follow the documented artifact lifecycle.
  Never delete active builds or another owner's artifacts.
- Before a push, run CI's own steps for what you touched: rebuild and byte-compare the checked
  components when the SDK or WIT changes ([provider change map](docs/development.md#provider-contract-or-host)),
  the audit-event documentation gate when you add a trace event, rustdoc with `-D warnings`, `cargo deny`.
- A fleet re-pin moves every exact pin a provider carries, not only `dekopon-provider-*`: providers
  also pin `dekopon-core`, `-capability`, `-broker`, `-broker-protocol` and `-broker-host` at the
  workspace version.
- A non-essential provider does not hold up a fleet rollout. When one is stuck (a release snag,
  a failing review, an owner-only step), ship without it and journal why it blocked.
- A provider named like an interface it imports takes a distinct package name
  (`dekopon:asset-provider` beside `dekopon:asset`).
- On macOS, a local Wasm build of a provider that links zstd needs `AR_wasm32_unknown_unknown`
  set to `llvm-ar`; the system `ar` writes an empty archive. Linux CI is unaffected.
- Verify a provider release by its release assets: the shared workflow attests the Wasm and SBOM
  files, so `gh attestation verify oci://…` returns 404 by design.
- Gitignored fixtures go stale in a fresh worktree after a fixture bump: a red local run that CI
  passes means refetch (`ci/fetch-external-provider-components.sh`) before bisecting.
- A wire-format, WIT or config-key change fixes the ship order before the code does: enumerate
  every consumer at its *deployed* version, verify each claim against the real artifact (the
  binary's imports, a decode of real data), and write the order into the PR. Each repo has its
  own release ritual; read its tags and release commits before trusting its README or automation.
- Report checks actually observed, exact head/artifact tested and verification gaps.
  Local tests do not prove deployed behavior or remote CI; never claim otherwise.
- Follow the [PR checklist](docs/development.md#before-opening-a-pull-request); required CI and review precede merge.
  The review is a human's, or under an owner-approved plan a fresh adversarial reviewer's.
  Automated agents never approve their own changes: the reviewer did not write the change.

## Rust guidelines

The tone of dekopon's Rust, for any agent that writes it. Where a rule and the surrounding code
disagree, match the code and say so in the PR. Each rule: one sentence of why, then the pair to
mirror.

### Authority

Contract surfaces stop and ask: WIT, wire frames and their fields, config keys and values, chart
values and mounts, what is deleted, the proof-gate invariants. Everything inside a crate is yours;
decide, mirror the nearest sibling, list it in your report under "Choices I made". Public crate
APIs are internals for now.

- Yes: pick `AssetInputs { rows, descriptors, sends_remaining }` as the host's parameter type and report it.
- No: `contact_supervisor("should the parameter be named AssetInputs or InvokeAssets?")`
- Yes: an ambiguous invariant read one way, implemented, and the reading listed under "Choices I made".
- No: a turn that ends with a question and no code.

### Errors

Typed variants are what callers match on to refuse; a string can only be printed.

- Yes:
  ```rust
  #[derive(Debug, Error)]
  pub enum AssetError {
      #[error("asset exceeds the per-asset ceiling")]
      TooLarge,
      #[error("could not read the asset")]
      Io { kind: io::ErrorKind },
  }
  ```
- No: `Err(format!("asset too large: {bytes}"))`, `anyhow!("read failed")`, `Io(io::Error)` across the broker boundary, `assert!(msg.contains("too large"))` in a test.
- Yes: an exhaustive `category()` on an error enum; callers match variants, never formatted strings.

### Dispatch

A closed enum makes the compiler find every match arm when the next kind arrives.

- Yes: `enum Source { File { fd: OwnedFd, cursor: u64, len: u64 } }` … `match source { Source::File { .. } => … }`
- No: `Box<dyn AssetSource>` with one implementer, or `trait Source { fn read(&mut self, …) }` plus generics threaded through every caller.
- Yes: an implementation declares its capabilities once and callers consult that declaration; content counting passes `TextUnit::Chars` or `TextUnit::Bytes` to the implementation, not a transport-name branch.
- Yes: a produced outcome is an enum with its label derived by exhaustive match, not a string used for dispatch.
- Yes: `match state { State::Open => true, State::Draining | State::Closed => false }` on an enum this crate defines.
- No: `matches!(state, State::Open)` on your own enum; it is a hidden `_ => false` the next variant slips past.
- Yes: an exhaustive `match (kind, shape)` with explicit `false` arms for permissions on enums this crate owns.

### Newtypes

Two `u64`s that mean different things must not be swappable.

- Yes: `struct AssetId(u64); struct DescriptorIndex(u32);`
- No: `fn admit(id: u64, descriptor: u64, bytes: u64)`
- Yes: a size limit is private to the type whose constructor enforces it, like `DeliveredTurnRequest::new` fitting the user text to the record.
- No: a bare `pub const` limit that another module compares against different content, like a delivered-turn bound reused on an encoded journal line.
- Yes: limits sourced from the implementation's capability and stored on the enforcing type, not repeated as caller constants.
- Yes: trust-boundary text or bytes wrapped in a newtype constructed only by its producer; platform coordinates use platform-specific ID types.

### Panics

The broker holds the credentials; one bad frame must not take it down.

- Yes: `let file = guard.as_mut().ok_or(BlobError::Reclaimed)?;`
- No: `guard.as_mut().expect("owner holds a file")`, `frame[0..4]` on peer bytes, `.unwrap()` on a socket.

### Bytes

Bytes stream through bounded readers and writers; a whole payload in a `Vec` is the peak this
change exists to remove.

- Yes: `io::copy(&mut DecoderReader::new(&mut spool, &STANDARD).take(CHUNK), &mut sink)`; `reqwest::Body::wrap(body)` with an exact `size_hint`; `Vec::with_capacity(MAX_CHUNK_BYTES)` reused.
- No: `let all = std::fs::read(path)?; let b64 = STANDARD.encode(&all);`, `wrap_stream`, a `Vec` that grows until the payload ends.

### Ownership

Own only what you store; a clone that satisfies the borrow checker is a design smell.

- Yes: `fn register(&mut self, content_type: &str, blob: DiskBlob)`; `Arc<Mutex<Table>>`; lock, copy the field out, unlock, then `.await`.
- No: `fn register(&mut self, content_type: String, blob: &DiskBlob) { … blob.clone() … }`; `Rc`; `let g = m.lock(); client.send(&*g).await`.
- Yes: fixed lifecycle steps encoded as types or consumed tickets, not a `finished: bool` checked at run time.

### Async

tokio is the runtime; the `bindgen!` host traits are already `async fn` in traits, so ours are too.

- Yes: `trait Sink { fn write(&mut self, chunk: &[u8]) -> impl Future<Output = io::Result<()>> + Send; }` where a spawn needs it; `tokio::task::spawn_blocking(move || blob.read())`.
- No: `#[async_trait]`, `Pin<Box<dyn Future<Output = …>>>` in a signature, `std::fs::read` inside an `async fn`.

### Concurrency

Every task, thread, queue and lock has an owner, a bound and a shutdown path. Before adding one,
name what already bounds the work (a session gate, `&mut self`, a lock that already serializes
it); if something does, add nothing, and prefer deleting a redundant primitive to adding a new one.
[`clippy.toml`](clippy.toml) bans the raw forms below through `disallowed-methods` and
`disallowed-types`, beside the workspace `await_holding_*` lints. A production use needs a site
`#[expect(clippy::disallowed_methods, reason = "owner: …; bound: …")]`: the reason is the registry,
and rustc fails an expectation that stops firing. Tests are exempt at each crate root.

- Yes: tasks in a `JoinSet` the owner joins at shutdown; `mpsc::channel(N)` with the full-queue policy named; `Arc::clone(&permits).try_acquire_owned()` **before** spawning, the permit moved into the task; `parking_lot::Mutex` for bookkeeping, locked, copied out and dropped; `watch` for latest state, `oneshot` for one reply; an awaited `spawn_blocking`, carrying its permit into the closure when the job can outlive its caller; `Handle::block_on` only on a blocking thread.
- No: `tokio::spawn` with a dropped `JoinHandle`; `unbounded_channel`; a `Condvar` drain; a lock held across `block_on`, network I/O or a channel wait; `tokio::sync::Mutex` for plain bookkeeping; `std::thread::spawn`; cancellation machinery for native work that is correct to let finish (a token rotation); a CI script, registry file or grep gate to enforce any of this.
- Yes: one owner of a terminal race's compare-and-swap, returning a `#[must_use]` result.

### Dependencies

The workspace already carries the mature crate; wrapping it is one function, re-implementing it is a second security boundary.

- Yes: `base64::write::EncoderWriter`, `http_body::Body`, `rustix::net::recvmsg`, `tokio::net::UnixStream::pair()`.
- No: a hand-rolled base64 table, a length-prefix framer beside `http_body`, a new crate without a sentence in the PR body.

### Tests

Don't version-control bitrotting assets like certificates; generate them in the test suite instead.

The name states the invariant and the primitives are real; one test per behaviour, not one per boundary.

- Yes: `fn a_rejected_frame_leaves_no_open_descriptors()` over `UnixStream::pair()`; `fn an_oversized_asset_is_refused()` asserting `matches!(err, AssetError::TooLarge)`; order and structure asserted, time driven by tokio's paused clock.
- No: `fn test_frame_2()`, `mockall::mock! { Broker }`, `assert!(err.to_string().contains("too large"))`, exactly-the-ceiling beside one-over twins, a 1 ns-over timeout cap, `assert!(elapsed < Duration::from_millis(50))`, production bytes canonicalized so a golden fixture is stable (compare parsed `Value`s instead).
- A test named for the real component loads the built artifact and reads its path with `expect`: no fallback file, early return or skip. A native-only test carries a native name.
- A real-component test asserts the request it caused: the full URI with any configured prefix, exactly one call, no guest-sent credential. Before claiming coverage, name what each harness observes and the payload sizes it ran at.
- An example's `#[cfg(test)]` module runs under `cargo test --lib --bins --tests` only when its `[[example]]` sets `test = true`.
- A fake that receives `Invoke` reads its frames with `DescriptorStream`, never plain `read(2)`: on macOS a passed `SCM_RIGHTS` descriptor stays open in the receiver (Linux closes it), so the pipe never sees EOF and the test hangs only on the Mac.
- Yes: one consumer test against a fake capability matrix and a shared conformance contract run for every implementation.

### Telemetry

A dashboard should never have to parse a string, and the store makes a column of every name.

- Yes: `http.response.body.size = 12288`; `url.query.keys = ["q", "page"]` beside `url.query.values = ["rust", "[redacted]"]`; the OpenTelemetry semantic-convention name where one exists; a job as a `job.started` and a `job.finished` record, linked; a rollup as a log record, which keeps its number types.
- No: `url.query.q = "rust"` (data as an attribute name); `"12 KiB"`; a JSON array inside a string; a map attribute; one span held open for a job's life (the store rejects a span that started over five hours ago); a provider argument that changes what is recorded about the provider.

### Wire formats

Providers add events, item types and fields without notice; a client that refuses them breaks on someone else's deploy.

- Yes: an unknown SSE event or response item is skipped with a trace; a known event with a malformed body is `ProtocolFailure`.
- No: `_ => return Err(ProtocolFailure::UnknownEvent(..))`; a test asserting an unknown event is fatal; pinning state to provider behaviour that no other client of the same API relies on.

### Size

The less code a change adds, the less there is to be wrong.

- Yes: the tight type (`NonZeroU32`) at the parse boundary; redaction as a substitution (strip controls, replace exact secrets longest-first, truncate, drop a trailing partial secret when the read was cut).
- No: `Option<i64>` so an invalid count can "join the semantic errors"; a byte budget on an error excerpt; redaction that blanks the whole body; a per-item linear scan or a buffer rescanned on every chunk.

### Comments

The owner reads the code; every comment is text he has to read past. Write none by default. A
comment earns its line only by stating a constraint the code cannot show: an ordering requirement,
an invariant other code relies on that the types do not enforce, the security reason behind a
restriction that looks arbitrary, an upstream workaround with its link, or why the obvious simpler
alternative is wrong here. One sentence, at the site. `///` and `//!` follow the same rule; the
exceptions are one-or-two-sentence docs on the public items of `dekopon-provider-sdk` and its
testkit, clap `///` (it is the `--help` text), `///` on `JsonSchema` input fields (it is the
model-facing schema description), `compile_fail` doctests, and `// SAFETY:`.

- Yes: `// The descriptor closes before accounting is released; unlink alone is not disk reclamation.`
- No: a `///` that restates the item's name or signature; a module overview; `// step 1: open the file`; `// handle error`; `// TODO: clean this up`; history (`// previously…`, `// now uses…`, `// replaces the old…`); plan, finding or PR IDs (`// D18`, `// W2-E`, `// see #187`); a comment narrating what a test asserts; three lines where one sentence carries the constraint.

### The tells you are writing Python in Rust

`Option<String>` where an enum belongs; `HashMap<String, serde_json::Value>` as a struct; a `bool`
parameter; behaviour selected by comparing strings; `Result<(), String>`; `.clone()` to end a
borrow; `Vec<u8>` handed whole between layers; a `&'static str` naming an outcome, reason or primitive that is later compared; `#[allow(clippy::too_many_arguments)]` or a tuple return of three or more items.

### Review checklist

How a verifier reads a change against the Rust guidelines, and how an editor reports one. The
verifier is the check that replaces pre-approval of crate internals, so it is never skipped and
never relaxed; the final PR reviewer has a different job and does not repeat it.

**Findings are tagged.** Every finding is one of `contract` (WIT, wire frames, config keys and
values, chart values and mounts, a deletion, a proof-gate invariant), `guideline` (a rule in this section,
quoted by heading) or `taste`. `contract` and `guideline` findings carry `file:line`, the concrete
failure and the exact fix, and make the verdict `FIX REQUIRED`. `taste` is advisory, listed last,
and never blocks.

**Evidence standard.** A finding names what it saw, not what it suspects; "this could leak" is not
a finding until the line that leaks is named. A claim in the editor's report ("mirrors
`DiskBlob::reclaim`") is checked, not trusted.

**Two fix passes.** An editor gets two resumed passes on `FIX REQUIRED`. A third `FIX REQUIRED`
reports the lane as blocked with both verdicts side by side; the disagreement is the owner's.

**The lane report** ends with one fixed heading, `Choices I made`: every place the editor read
the brief's intent over its text and did something the text did not say, one line each with the
sentence it overrode.

**Comment findings.** Every added or edited comment is read against Comments. One that does not
state a constraint the code cannot show is a `guideline` finding, and the fix is deletion, not
rewording.

**Concurrency findings** name the owner and bound the change is missing, or the existing owner that
makes a new primitive redundant; "this could race" is not a finding until the interleaving is named.

**The PR reviewer** reads the assembled change for what only the whole shows: one definition per
fact across lanes, seams matching on both sides, deletions complete, docs describing only the new
behaviour, `Changelog:` commit lines present (`Fixed` only for bugs in released code), work that grows with
a stream, wire parsers that refuse the unknown. It does not re-run the per-lane rubric.
