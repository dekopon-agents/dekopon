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

### Dependencies

The workspace already carries the mature crate; wrapping it is one function, re-implementing it is a second security boundary.

- Yes: `base64::write::EncoderWriter`, `http_body::Body`, `rustix::net::recvmsg`, `tokio::net::UnixStream::pair()`.
- No: a hand-rolled base64 table, a length-prefix framer beside `http_body`, a new crate without a sentence in the PR body.

### Tests

The name states the invariant and the primitives are real; one test per behaviour, not one per boundary.

- Yes: `fn a_rejected_frame_leaves_no_open_descriptors()` over `UnixStream::pair()`; `fn an_oversized_asset_is_refused()` asserting `matches!(err, AssetError::TooLarge)`; order and structure asserted, time driven by tokio's paused clock.
- No: `fn test_frame_2()`, `mockall::mock! { Broker }`, `assert!(err.to_string().contains("too large"))`, exactly-the-ceiling beside one-over twins, a 1 ns-over timeout cap, `assert!(elapsed < Duration::from_millis(50))`, production bytes canonicalized so a golden fixture is stable (compare parsed `Value`s instead).
- An example's `#[cfg(test)]` module runs under `cargo test --lib --bins --tests` only when its `[[example]]` sets `test = true`.

### Wire formats

Providers add events, item types and fields without notice; a client that refuses them breaks on someone else's deploy.

- Yes: an unknown SSE event or response item is skipped with a trace; a known event with a malformed body is `ProtocolFailure`.
- No: `_ => return Err(ProtocolFailure::UnknownEvent(..))`; a test asserting an unknown event is fatal; pinning state to provider behaviour that no other client of the same API relies on.

### Size

The less code a change adds, the less there is to be wrong.

- Yes: the tight type (`NonZeroU32`) at the parse boundary; redaction as a substitution (strip controls, replace exact secrets longest-first, truncate, drop a trailing partial secret when the read was cut).
- No: `Option<i64>` so an invalid count can "join the semantic errors"; a byte budget on an error excerpt; redaction that blanks the whole body; a per-item linear scan or a buffer rescanned on every chunk.

### Comments

A comment says why or states the invariant; the code already says what.

- Yes: `// The descriptor closes before accounting is released; unlink alone is not disk reclamation.`
- No: `// step 1: open the file`, `// handle error`, `// TODO: clean this up`.

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

**The lane report** ends with one fixed heading, `Choices I made`: every place the editor read
the brief's intent over its text and did something the text did not say, one line each with the
sentence it overrode.

**The PR reviewer** reads the assembled change for what only the whole shows: one definition per
fact across lanes, seams matching on both sides, deletions complete, docs describing only the new
behaviour, CHANGELOG bullets present (`Fixed` only for bugs in released code), work that grows with
a stream, wire parsers that refuse the unknown. It does not re-run the per-lane rubric.
