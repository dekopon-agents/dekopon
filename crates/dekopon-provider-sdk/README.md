# dekopon-provider-sdk

Rust guest SDK for Dekopon WebAssembly component providers.

**Start here:** [Build and run an import-free Wasm provider with Rust](https://dekopon-agents.github.io/guides/provider-sdk/) is a reproducible walkthrough of the provider contract. Follow its exact pins rather than mixing versions across releases.

Providers bundled with Dekopon consume this same public SDK and runtime contract; they are ordinary components, not privileged plugins.

Implement the `Provider` trait — `manifest`, `invoke`, and `run_command` — declare at least one command word in the manifest's `commandWords`, generate bindings for a world including `dekopon:provider/provider-cli@0.3.0`, and call `export_provider_with_cli!` once. A model reaches a provider only through its command words: `gh pr view 7` runs `run_command`, which proposes a capability the broker authorizes and then executes through `invoke`. A broker refuses to start with a provider that declares capabilities and no command word, naming it in the same report as every other provider-set conflict. The generated adapter decodes JSON at the component boundary and turns provider errors into a typed wire response. The host requires object-shaped inputs but does not generally enforce each capability's JSON Schema; provider implementations validate their own required fields, types, and constraints.

```wit
world provider {
    include dekopon:provider/provider-cli@0.3.0;
}
```

```rust,ignore
use dekopon_provider_sdk::{CommandRun, Provider, ProviderError, ProviderManifest};

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

struct Example;

impl Provider for Example {
    fn manifest() -> ProviderManifest { /* ... declares `command_words` ... */ }
    fn invoke(/* ... */) -> Result<serde_json::Value, ProviderError> { /* ... */ }
    fn run_command(argv: &[String], stdin: Option<&str>) -> Result<CommandRun, ProviderError> { /* ... */ }
}

dekopon_provider_sdk::export_provider_with_cli!(Example, bindings);
```

The SDK owns the canonical provider WIT in [`wit/provider.wit`](wit/provider.wit). Update every surviving broker and guest mirror together; the mirror list and equality gates are in `docs/development.md`.

[Command-line providers](#command-line-providers) covers `run_command` itself — help pages, usage errors, stdin, proposals, and secret uses, hand-rolled or through the optional [`clap` layer](#the-clap-layer).

## Provider-owned worlds

A provider that needs a broker service adds the import to its own composed world beside the `provider-cli` exports, and hands the bindings it generates to the same `export_provider_with_cli!`:

```wit
world provider {
    include dekopon:provider/provider-cli@0.3.0;
    import dekopon:asset/asset@0.1.0;
    import dekopon:http/client@1.1.0;
}
```

Additional imports are embedded in the component type and fail closed unless an authorized broker linker implements them. See the [`http-probe`](../../examples/providers/http-probe/README.md) fixture for the HTTP import, and [`clock-probe`](../../examples/providers/clock-probe/README.md) for `dekopon:clock/wall@1.0.0`. Host imports are for `invoke`: `run-command` stays pure, and a broker refuses a component that reaches for one there.

## Assets out of band

Asset bytes never enter a model transcript, proposal, result JSON, or broker frame. Proposals keep
`chat-asset:<N>` string references; the gateway passes read-only descriptors separately. During
`invoke`, `asset::open` resolves only references passed with that invocation. `asset::list` returns
conversation metadata, not authority to open an unreferenced asset.

The `asset` module wraps `dekopon:asset/asset@0.1.0`. `Handle::read` and `read_at` read at most 64 KiB
of decoded bytes per call; `read_all` reserves the stored length when known and otherwise grows as
it reads. `asset::allocate(content_type, encoding)` creates a writer, and `Writer::write_all`
appends stored bytes in chunks of at most 64 KiB. Identity and canonical base64 are storage
encodings, not content types. The host performs encoding conversion.

`asset::attach(writer)` consumes the writer and joins the conversation's temp files; it does **not**
send them. The gateway numbers successful outputs and adds a bounded reference, content-type and
size note to the command output. Return only application metadata in the ordinary JSON result.
`asset::send(&handle)` separately marks delivery on this turn's reply; `remove` deletes an unsent
asset. Attach, remove and send require their own broker grants. A dropped unattached writer is
discarded, and failed invocations do not contribute outputs.

For HTTP bodies, `dekopon-provider-http::StreamedRequest` composes ordered `Part::literal` and
`Part::asset` segments. The host streams asset parts at exact length without loading them into the
guest and returns an echo-scanned response `Handle`. Buffered HTTP `send` remains available for
small requests. Import the asset interface beside HTTP as above; imports are available only in
`invoke`, never the pure `run-command` phase.

## Host feature

Providers never enable it, and the default feature set is empty, so a `wasm32-unknown-unknown` build
never compiles it. The optional `host` feature adds `dekopon_provider_sdk::host`: the Wasmtime
plumbing consumed by `dekopon-broker-host` and external embeddings — manifest validation, the report
a whole provider set fails with (duplicated identities, colliding command words, and providers
declaring capabilities with no command word), the bounds on one store, the engine constructor, and
the command-export plumbing: `command_export` reads whether a compiled component offers
`run-command` and with what type, `check_command_export` is the load gate a manifest declaring
`commandWords` must pass, and `command_input_bytes` is what a host counts against its input bound
for one run. It pulls in Wasmtime. Each host owns its own linker and its own way of interrupting a guest that runs too
long.

## WIT package

The contract is published as `dekopon:provider@0.3.0`. Fetch it through Dekopon's public registry metadata:

```console
wkg get \
  --registry dekopon-agents.github.io \
  --output provider.wasm \
  dekopon:provider@0.3.0
```

The published `0.3.0` package contains three worlds and no imports: `provider` exports exactly `describe` and `invoke`; `provider-cli` includes it and adds `run-command`; a third world, `provider-commands`, adds a `resolve-command` export this SDK no longer generates and no host still calls. Published package versions are immutable, so both of the other worlds stay in the text. Build against `provider-cli`: a component built against `provider` alone, or against `dekopon:provider@0.1.0` or `@0.2.0`, exports no `run-command`, so it cannot serve the command words a broker requires of every provider and no longer loads. The published package is an authoring contract; it adds no host function and no runtime authority.

## Command-line providers

A provider's command words behave like the upstream command-line tool: `gh --help` renders a help page on stdout at status 0, `gh bogus` prints a usage error on stderr at status 2, `gh pr view 7` proposes `gh.pull-request.read`, and `echo '{…}' | gh api --input -` receives the piped value.

`run_command` returns one of three things. `CommandRun::Proposal` is a capability proposal and is authorized on the same path as any other; `CommandRun::Rendered` is text the guest produced by itself, with separate stdout and stderr and an exit status, so the shell's two streams map one to one (`$(gh bogus)` captures nothing while the error reaches the model); `Err(ProviderError)` is a decline, reported as a usage error. Two paths implement it, and the hand-rolled one is the contract the `clap` layer builds on.

A proposal may also name one secret use: `CommandInvocation::secret_use` takes a `SecretUseProposal` (re-exported beside `SecretDrn`) — a public DRN and the native sink the broker renders it in, `httpBearer` or `httpBasic`. It crosses the boundary as `secretUse` beside `capability` and `input`, and is absent when `None`, so a proposal naming no secret keeps its earlier shape. The broker authorizes the secret use separately and matches an owner-authored binding exactly as for any other; the secret bytes never reach the component. `CommandRun::proposal` names none.

### The hand-rolled baseline

The trait needs no argument parser — match on argv slices and shift values out by hand:

```rust,ignore
use dekopon_provider_sdk::{CommandRun, Provider, ProviderError};

const HELP: &str = "Usage: memory recent --last N\n       memory search [-]\n";

fn run_command(argv: &[String], stdin: Option<&str>) -> Result<CommandRun, ProviderError> {
    match argv {
        [flag] if flag == "--help" => Ok(CommandRun::rendered(HELP, 0)),
        [word, flag, last] if word == "recent" && flag == "--last" => {
            let last: u32 = last
                .parse()
                .map_err(|error| ProviderError::new("usage", format!("--last: {error}")))?;
            Ok(CommandRun::proposal(MEMORY_RECENT, serde_json::json!({ "last": last })))
        }
        [word, dash] if word == "search" && dash == "-" => match stdin {
            Some(query) => Ok(CommandRun::proposal(MEMORY_SEARCH, serde_json::json!({ "query": query }))),
            None => Ok(CommandRun::rendered_error("memory search -: nothing was piped in\n", 2)),
        },
        _ => Ok(CommandRun::rendered_error(HELP, 2)),
    }
}

dekopon_provider_sdk::export_provider_with_cli!(Example, bindings);
```

Keep each capability identifier in one `const` used by both `manifest()` and `run_command`, so renaming one is a compile error rather than an exit code a model discovers mid-session. `stdin` is `None` when nothing was piped into the word. A proposal is pure and grants nothing: it is authorized on the path every capability takes — constraint-set lookup, Cedar evaluation, credential injection at the native HTTP boundary — so naming a capability the caller was not granted produces a denial rather than an escalation. Rendered text authorizes nothing either, and both are produced before authorization, so neither may touch a host import. Declaring `commandWords` without exporting `run-command` is refused at load, declaring capabilities with no command word refuses to start, and a word colliding with a shell builtin, a refused or control word, or another provider's word is a startup failure; one report names every such conflict at once. The [`memory-reservation-probe`](../../examples/providers/memory-reservation-probe/README.md) fixture is this path checked in.

### The `clap` layer

The recommended way to write the same thing. Enable the SDK's `clap` feature and declare the command tree once; `dekopon_provider_sdk::cli::run_command` parses the argv against it and does what the upstream tool's `main` would: `--help`, `--version`, and the `help` subcommand render on stdout at status 0, an unknown subcommand, a missing argument, or a refused value renders clap's own usage error on stderr at status 2, and a well-formed argv reaches a dispatch closure with the piped value, whose proposal is authorized as any other. The SDK re-exports `clap`, so a guest builds its tree — by hand or with `#[derive(Parser)]` — against the SDK's exact version without declaring the dependency:

```toml
[dependencies]
dekopon-provider-sdk = { version = "0.17.0", features = ["clap"] }
```

```rust,ignore
use dekopon_provider_sdk::clap::{Arg, ArgMatches, Command};
use dekopon_provider_sdk::{CommandInvocation, CommandRun, ProviderError, cli};

const PR_READ: &str = "gh.pull-request.read";

fn tree() -> Command {
    Command::new("gh").version("0.1.0").subcommand_required(true).subcommand(
        Command::new("pr").subcommand_required(true).subcommand(
            Command::new("view").about("View a pull request").arg(Arg::new("number").required(true)),
        ),
    )
}

fn dispatch(matches: ArgMatches, stdin: Option<&str>) -> Result<CommandInvocation, ProviderError> {
    match matches.subcommand() {
        Some(("pr", pr)) => match pr.subcommand() {
            Some(("view", view)) => Ok(CommandInvocation {
                capability: PR_READ.parse().expect("static capability ID"),
                input: serde_json::json!({ "number": view.get_one::<String>("number") }),
                secret_use: None,
            }),
            _ => Err(ProviderError::new("usage", "gh pr view <NUMBER>")),
        },
        _ => Err(ProviderError::new("usage", "gh pr <COMMAND>")),
    }
}

fn run_command(argv: &[String], stdin: Option<&str>) -> Result<CommandRun, ProviderError> {
    cli::run_command(tree(), argv, stdin, dispatch)
}
```

The same `const`-per-capability convention applies: `manifest()` and `dispatch` read one identifier, so a rename is a compile error, and a fixture test that walks every dispatch target and finds it in the manifest closes the remaining gap. The tree is built on every call — a command word runs in a fresh store under a fuel bound, and there is no process-lifetime static to hold it — so keep it declarative. What clap cannot know (whether anything was piped into `-`, a bound on a value) is the dispatch closure's to refuse, as a decline naming its cause.

The SDK's clap feature set is narrow: `std`, `help`, `usage`, `error-context`, and `derive`, declared directly rather than inherited from the workspace so that two features never reach a guest. `env` would let an argument default from a process environment a component does not have and must never read; `color` pulls in a terminal probe and would put escape sequences in text a model reads. The layer never calls `get_matches` (which reads `std::env::args_os`), `Error::exit`, or `Error::print`; rendered text is returned, never printed. The [`cli-probe`](../../examples/providers/cli-probe/README.md) fixture is this path checked in, with clap's exact help page pinned by its lockfile, and [`http-probe`](../../examples/providers/http-probe/README.md) is the same layer over a world with a host import.
