# Dekopon

Dekopon runs AI agents on hardware you own and lets them act on real systems without ever holding
the keys. An agent answers in Slack, Discord, Telegram or WhatsApp, works by writing short bash-like
scripts, and reaches GitHub or any HTTP API through WebAssembly providers. A separate broker
decides, for the person asking, what each command may do, and only the broker holds credentials. A
prompt injection can still steer the model, but it reaches only what the broker allows the person
who asked.

Install on [Kubernetes](docs/deploy.md#kubernetes), the preferred deployment, or [on a
Mac](docs/deploy.md#mac) if you are moving from OpenClaw or Hermes.

**Status:** pre-1.0, with breaking changes between releases. It runs the maintainer's Raspberry Pi,
serving a family's chats; it makes no production-hardening claim. Both daemons are Unix-only.

## Four ideas

**Credentials the agent never holds.** The model proposes through one script tool; a command word in
it is a request, not a process. `dekopon-brokerd`, a separate process under its own UID, runs the
provider in a fresh Wasm sandbox, and after the provider builds an HTTP request the broker adds a
credential bound to that destination. The model, the script, the gateway and the provider never see
it, and the broker refuses a response that echoes it back. The endpoint does receive it, so trust
the hosts you bind a credential to. See the [design](docs/design.md).

**Two-layer authority.** Cedar decides who may act: the gateway attests which chat sender asked, and
only the broker maps that sender to a principal. Constraint sets the owner writes decide how far
(hosts, methods, request and byte ceilings, the credential), and no policy can widen them. Cedar
also checks each outbound request a provider makes. See [authorization](docs/authorization.md).

**The memory ladder.** Conversations replay from a journal through summaries written ahead of time,
and older turns keep their place between repacks, so consecutive prompts share a prefix and the
provider's prompt cache stays warm. The model can read any original back. Memory is prompt text and
never grants authority. See [memory](docs/memory.md).

**Keyless model access for outside VMs.** Coding agents such as `claude`, `pi` or `codex` in a
Firecracker VM reach models through the gateway's proxy, which injects the key and charges the
agent's token budget. The VM never holds a model key; the proxy trusts the jail that connects over
mTLS to say which VM it serves.

## Install

Kubernetes, preferred:

```console
helm upgrade --install dekopon oci://ghcr.io/dekopon-agents/charts/dekopon \
  -n dekopon --create-namespace -f my-values.yaml
```

A Mac, for people moving from OpenClaw or Hermes:

```console
brew install dekopon-agents/tap/dekopon
brew pin dekopon
```

[Release archives](https://github.com/dekopon-agents/dekopon/releases) cover Apple silicon and Linux
on ARM64 and x86-64. Nothing starts until you write each daemon its configuration;
[`examples/conditional-write`](examples/conditional-write/) is a complete set, and
[deploy](docs/deploy.md) covers accounts, credentials, the Slack app and upgrades for both paths.
Neither the archives nor the formula include providers; each comes from its own release.

## Contributing

[`CONTRIBUTING.md`](CONTRIBUTING.md) and [development](docs/development.md) cover changes;
[`SECURITY.md`](SECURITY.md) covers reporting a vulnerability; the [Code of
Conduct](CODE_OF_CONDUCT.md) applies everywhere. Dekopon is licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.
