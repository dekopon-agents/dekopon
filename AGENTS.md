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
- Report checks actually observed, exact head/artifact tested and verification gaps.
  Local tests do not prove deployed behavior or remote CI; never claim otherwise.
- Follow the [PR checklist](docs/development.md#before-opening-a-pull-request); required CI and human review precede merge.
  Automated agents never approve their own changes.

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

### Dispatch

A closed enum makes the compiler find every match arm when the next kind arrives.

- Yes: `enum Source { File { fd: OwnedFd, cursor: u64, len: u64 } }` … `match source { Source::File { .. } => … }`
- No: `Box<dyn AssetSource>` with one implementer, or `trait Source { fn read(&mut self, …) }` plus generics threaded through every caller.

### Newtypes

Two `u64`s that mean different things must not be swappable.

- Yes: `struct AssetId(u64); struct DescriptorIndex(u32);`
- No: `fn admit(id: u64, descriptor: u64, bytes: u64)`

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

- Yes: tasks in a `JoinSet` the owner joins at shutdown; `mpsc::channel(N)` with the full-queue policy named; `Arc::clone(&permits).try_acquire_owned()` **before** spawning, the permit moved into the task; `std::sync::Mutex` for bookkeeping, locked, copied out and dropped; `watch` for latest state, `oneshot` for one reply; an awaited `spawn_blocking`, carrying its permit into the closure when the job can outlive its caller; `Handle::block_on` only on a blocking thread.
- No: `tokio::spawn` with a dropped `JoinHandle`; `unbounded_channel`; a `Condvar` drain; a lock held across `block_on`, network I/O or a channel wait; `tokio::sync::Mutex` for plain bookkeeping; `std::thread::spawn`; cancellation machinery for native work that is correct to let finish (a token rotation); a CI script, registry file or grep gate to enforce any of this.

### Dependencies

The workspace already carries the mature crate; wrapping it is one function, re-implementing it is a second security boundary.

- Yes: `base64::write::EncoderWriter`, `http_body::Body`, `rustix::net::recvmsg`, `tokio::net::UnixStream::pair()`.
- No: a hand-rolled base64 table, a length-prefix framer beside `http_body`, a new crate without a sentence in the PR body.

### Tests

The name states the invariant, the primitives are real, and every limit is tested at the edge and one past it.

- Yes: `fn a_rejected_frame_leaves_no_open_descriptors()` over `UnixStream::pair()`; `fn an_asset_of_exactly_the_ceiling_is_accepted()` beside `fn one_byte_over_the_ceiling_is_refused()` asserting `matches!(err, AssetError::TooLarge)`.
- No: `fn test_frame_2()`, `mockall::mock! { Broker }`, `assert!(err.to_string().contains("too large"))`.

### Comments

The owner reads the code; every comment is text he has to read past. Write none by default. A
comment earns its line only by stating a constraint the code cannot show: an ordering requirement,
an invariant other code relies on that the types do not enforce, the security reason behind a
restriction that looks arbitrary, an upstream workaround with its link, or why the obvious simpler
alternative is wrong here. One sentence, at the site. `///` and `//!` follow the same rule; the
exceptions are one-or-two-sentence docs on the public items of `dekopon-provider-sdk` and its
testkit, clap `///` (it is the `--help` text), `compile_fail` doctests, and `// SAFETY:`.

- Yes: `// The descriptor closes before accounting is released; unlink alone is not disk reclamation.`
- No: a `///` that restates the item's name or signature; a module overview; `// step 1: open the file`; `// handle error`; `// TODO: clean this up`; history (`// previously…`, `// now uses…`, `// replaces the old…`); plan, finding or PR IDs (`// D18`, `// W2-E`, `// see #187`); a comment narrating what a test asserts; three lines where one sentence carries the constraint.

### The tells you are writing Python in Rust

`Option<String>` where an enum belongs; `HashMap<String, serde_json::Value>` as a struct; a `bool`
parameter; behaviour selected by comparing strings; `Result<(), String>`; `.clone()` to end a
borrow; `Vec<u8>` handed whole between layers.

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

**The lane report** ends with two fixed headings. `Choices I made`: every place the editor read
the brief's intent over its text and did something the text did not say, one line each with the
sentence it overrode. `Limits`: one table of every ceiling constant the lane added or moved: name,
value, the test that hits it and the test one past it.

**Comment findings.** Every added or edited comment is read against Comments. One that does not
state a constraint the code cannot show is a `guideline` finding, and the fix is deletion, not
rewording.

**Concurrency findings** name the owner and bound the change is missing, or the existing owner that
makes a new primitive redundant; "this could race" is not a finding until the interleaving is named.

**The PR reviewer** reads the assembled change for what only the whole shows: one definition per
fact across lanes, seams matching on both sides, deletions complete, docs describing only the new
behaviour, CHANGELOG bullets present. It does not re-run the per-lane rubric.
