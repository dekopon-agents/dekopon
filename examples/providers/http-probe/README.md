# HTTP import probe

A test fixture for the broker host's `dekopon:http/client@1.0.0` import, not a provider to deploy. It composes the `dekopon:provider@0.3.0` `provider-cli` world with that import, and proves caller-generated provider worlds and the broker component host's authorized HTTP path.

Its word is `httpprobe`, built on the SDK's `clap` layer: one subcommand per capability and one flag per input field, the flag being the field's kebab-case spelling. The dispatch assembles exactly the input object `invoke` reads, and an optional field is present only when its flag was given.

- `httpprobe fetch --uri <URI> [--method <METHOD>] [--header <NAME> <VALUE>]... [--body <BODY>] [--catch-error] [--bearer <DRN> | --basic <USER> <DRN>]` proposes `http-probe.fetch`: the required `uri` plus an optional arbitrary method token, ordered text headers, and a buffered text body. The test-only `--catch-error` (`catchError`) demonstrates that guest code cannot mask a policy rejection. `--bearer` and `--basic` are not input fields: each proposes secret use, returned as the proposal's `secretUse` (`httpBearer`, or `httpBasic` with the username) and never placed in the input. The value is the bare public DRN, `drn:<authority>:secret:<realm>:<path>`, passed on argv as [Agent syntax](../../../docs/secrets.md#agent-syntax) describes, not the `${drn:…}` marker the retired `curl` builtin took. The broker authorizes it like any secret use: a `secret.use` policy decision plus a private binding matching the DRN, sink, and username.
- `httpprobe conditional-write --uri <URI> [--expected-etag <ETAG>]` proposes `http-probe.conditional-write`, the two-call capability: it reads the resource, then writes only if the etag it observed is unchanged, refusing in between. It exists so the broker host has an in-tree capability that makes *two* authorized calls in one invocation, which is what exercises `maxRequests`, per-call evidence, and the host-call limit.
- `httpprobe purge --uri <URI>` proposes `http-probe.purge`, which deletes one resource and exists so the manifest exposes something [`../../conditional-write/`](../../conditional-write/README.md) grants nowhere.

`httpprobe --help` and each subcommand's `--help` render clap's page on stdout at status 0; a missing `--uri`, an unknown subcommand, a `--header` or `--basic` short of its two values, or `--bearer` beside `--basic` renders clap's usage error on stderr at status 2. A non-canonical DRN or an invalid Basic username (empty, oversized, or containing `:` or a control character) is the guest's own decline naming the flag, which the shell reports as a usage error at exit 2. Broker-host tests authorize only an ephemeral loopback mock server; they never contact the public internet. The component requires the broker-owned HTTP import; importing it grants no authority.

Run native checks:

```console
cargo fmt --manifest-path examples/providers/http-probe/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/http-probe/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/http-probe/Cargo.toml
```

Build and inspect the generated component:

```console
examples/providers/http-probe/build.sh
wasm-tools validate examples/providers/http-probe-provider.wasm
wasm-tools component wit examples/providers/http-probe-provider.wasm
```

The decoded component must export exactly `describe`, `invoke`, and `run-command`, import exactly `dekopon:http/client@1.0.0`, and import no WASI interfaces.
