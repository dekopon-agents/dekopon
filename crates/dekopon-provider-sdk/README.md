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

## Provider-owned worlds

The default `export_provider!` macro targets the SDK's import-free world. A provider that needs a broker service generates bindings from its own composed world and supplies that module to `export_provider_with_bindings!`:

```wit
world provider {
    include dekopon:provider/provider@0.2.0;
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

## WIT package

The same import-free world is published as `dekopon:provider@0.2.0`, alongside a `provider-commands` world adding the optional `resolve-command` export. Fetch it through Dekopon's public registry metadata:

```console
wkg get \
  --registry dekopon-agents.github.io \
  --output provider.wasm \
  dekopon:provider@0.2.0
```

The package contains two worlds and no imports: `provider` exports exactly `describe` and `invoke`, and `provider-commands` includes it and adds `resolve-command`. They are separate so a host can require the base contract and treat the rewrite as optional, which keeps a component built against `dekopon:provider@0.1.0` loadable. Publishing the package makes the existing authoring contract available to component tooling; it does not add host functions or runtime authority.

## Command words

A provider can contribute bare words to the sandboxed shell — `memory recent --last 5` instead of `cap memory.chat.recent '{"last":5}'`. Declare them in the manifest's `commandWords`, implement `Provider::resolve_command`, generate bindings for a world including `dekopon:provider/provider-commands@0.2.0`, and export with `export_provider_with_commands!`:

```wit
world provider {
    include dekopon:provider/provider-commands@0.2.0;
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

The rewrite is pure and grants nothing: it returns a proposal that is authorized on exactly the path a direct `cap <id> {…}` call takes, so naming a capability the caller was not granted produces a denial rather than an escalation. It runs before authorization and must not touch a host import. Declaring `commandWords` without exporting `resolve-command` is refused at load, and a word colliding with a shell builtin, a refused or control word, or another provider's word is a startup failure that names every conflict at once.
