# dekopon-provider-sdk

Rust guest SDK for Dekopon WebAssembly component providers.

**Start here:** [Build and run an import-free Wasm provider with Rust](https://dekopon-agents.github.io/guides/provider-sdk/) is a reproducible walkthrough pinned to v0.7.0. Every release since keeps the same provider contract, so the walkthrough still applies to this tree; follow the guide's exact pins rather than mixing release versions.

Providers bundled with Dekopon consume this same public SDK and runtime contract; they are ordinary components, not privileged plugins.

Implement the `Provider` trait and call `export_provider!` once. The generated adapter exports the WIT world in [`wit/provider.wit`](wit/provider.wit), decodes JSON at the component boundary, and turns provider errors into a typed wire response. The host requires object-shaped inputs but does not generally enforce each capability's JSON Schema; provider implementations validate their own required fields, types, and constraints.

```rust,ignore
use dekopon_provider_sdk::{Provider, ProviderError, ProviderManifest};

struct Example;

impl Provider for Example {
    fn manifest() -> ProviderManifest { /* ... */ }
    fn invoke(/* ... */) -> Result<serde_json::Value, ProviderError> { /* ... */ }
}

dekopon_provider_sdk::export_provider!(Example);
```

The immediate host accepts only read-only manifests and supplies no WASI imports. The SDK WIT file is mirrored by `dekopon-provider-host`; update both copies together and keep their equality test passing.

Two more paths exist for a provider that contributes words to the sandboxed shell: [command-line providers](#command-line-providers) (`run-command`, the current contract: help pages, usage errors, stdin, and proposals) and the legacy [command words](#command-words) rewrite (`resolve-command`).

## Provider-owned worlds

The default `export_provider!` macro targets the SDK's import-free world. A provider that needs a broker service generates bindings from its own composed world and supplies that module to `export_provider_with_bindings!`:

```wit
world provider {
    include dekopon:provider/provider@0.3.0;
    import dekopon:http/client@1.0.0;
}
```

```rust,ignore
mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider",
        generate_all,
        pub_export_macro: true,
    });
}

dekopon_provider_sdk::export_provider_with_bindings!(Example, bindings);
```

The composed world must retain the root `describe` and `invoke` exports. Additional imports are embedded in the component type and fail closed unless an authorized broker linker implements them. The direct `dekopon-run` host remains empty and rejects such components; see the [`http-probe`](../../examples/providers/http-probe/README.md) fixture.

## Host feature

Providers never enable it, and the default feature set is empty, so a `wasm32-unknown-unknown` build
never compiles it. The optional `host` feature adds `dekopon_provider_sdk::host`: the Wasmtime
plumbing `dekopon-provider-host` and `dekopon-broker-host` both need — manifest validation behind an
effect gate, the report a whole conflicting provider set fails with, the bounds on one store, the
engine constructor, and the command-export plumbing: `command_export` reads which of `run-command`
and `resolve-command` a compiled component offers (the newer one wins when both exist),
`check_command_export` is the load gate a manifest declaring `commandWords` must pass,
`command_input_bytes` is what a host counts against its input bound for one run, and
`parse_command_run` decodes either export's answer into one `CommandRunOutcome`. It pulls in
Wasmtime. Each host still owns its own linker and its own way of interrupting a guest that runs too
long.

## WIT package

The same import-free world is published as `dekopon:provider@0.3.0`, alongside a `provider-cli` world adding the optional `run-command` export and a `provider-commands` world adding the legacy `resolve-command` export. Fetch it through Dekopon's public registry metadata:

```console
wkg get \
  --registry dekopon-agents.github.io \
  --output provider.wasm \
  dekopon:provider@0.3.0
```

The package contains three worlds and no imports: `provider` exports exactly `describe` and `invoke`; `provider-commands` includes it and adds `resolve-command`; `provider-cli` includes it and adds `run-command`. They are separate so a host can require the base contract and look the command export up by name, which keeps components built against `dekopon:provider@0.1.0` and `@0.2.0` loadable and a `0.2.0` `resolve-command` guest working on the same shell path. A host that finds both exports calls `run-command`. Publishing the package makes the existing authoring contract available to component tooling; it does not add host functions or runtime authority.

## Command-line providers

A provider's command words can behave like the upstream command-line tool: `gh --help` renders a help page on stdout at status 0, `gh bogus` prints a usage error on stderr at status 2, `gh pr view 7` proposes `gh.pull-request.read` exactly as before, and `echo '{…}' | gh api --input -` receives the piped value. Declare the words in the manifest's `commandWords`, implement `Provider::run_command`, generate bindings for a world including `dekopon:provider/provider-cli@0.3.0`, and export with `export_provider_with_cli!`:

```wit
world provider {
    include dekopon:provider/provider-cli@0.3.0;
}
```

`run_command` returns one of three things. `CommandRun::Proposal` is a capability proposal and is authorized on the same path as any other; `CommandRun::Rendered` is text the guest produced by itself, with separate stdout and stderr and an exit status, so the shell's two streams map one to one (`$(gh bogus)` captures nothing while the error still reaches the model); `Err(ProviderError)` is a decline, reported as a usage error. The trait needs no argument parser — match on argv slices and shift values out by hand:

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

Keep each capability identifier in one `const` used by both `manifest()` and `run_command`, so renaming one is a compile error rather than an exit code a model discovers mid-session. `stdin` is `None` when nothing was piped into the word. Rendered text authorizes nothing and is produced before authorization, so the same rule as the rewrite applies: pure, and no host imports. A `clap`-based layer that renders help and usage errors from a declared command tree is a later addition; the hand-rolled path above is the contract it will build on.

The default `run_command` delegates to `resolve_command`, so a provider written against the legacy rewrite can move to `export_provider_with_cli!` and the `provider-cli` world without changing anything else; it then gains no stdin until it implements `run_command`, because the legacy contract has none.

## Command words

The legacy form of the same thing: a provider contributes bare words to the sandboxed shell — `memory recent --last 5` instead of `cap memory.chat.recent '{"last":5}'` — and can only rewrite an argv into a proposal or decline it; it cannot render help and receives no stdin. Declare the words in the manifest's `commandWords`, implement `Provider::resolve_command`, generate bindings for a world including `dekopon:provider/provider-commands@0.3.0`, and export with `export_provider_with_commands!`:

```wit
world provider {
    include dekopon:provider/provider-commands@0.3.0;
}
```

```rust,ignore
fn manifest() -> ProviderManifest {
    ProviderManifest {
        command_words: vec!["memory".to_owned()],
        // ...
    }
}

fn resolve_command(argv: &[String]) -> Result<CommandInvocation, ProviderError> {
    // `argv` holds the arguments after the word; the word itself is already selected.
    match argv {
        [operation, flag, last] if operation == "recent" && flag == "--last" => { /* ... */ }
        _ => Err(ProviderError::new("usage", "memory recent --last N")),
    }
}

dekopon_provider_sdk::export_provider_with_commands!(Example, bindings);
```

The rewrite is pure and grants nothing: it returns a proposal that is authorized on exactly the path a direct `cap <id> {…}` call takes, so naming a capability the caller was not granted produces a denial rather than an escalation. It runs before authorization and must not touch a host import. Declaring `commandWords` without exporting `run-command` or `resolve-command` is refused at load, and a word colliding with a shell builtin, a refused or control word, or another provider's word is a startup failure that names every conflict at once. Components built against `provider-commands@0.2.0` keep loading and keep working unchanged.
