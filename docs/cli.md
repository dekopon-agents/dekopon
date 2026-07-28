# CLI reference

`dekopon` is the local catalog operator interface. Version `0.1.0` is synchronous and reads a validated local catalog; it does not contact a daemon. The separate experimental `dekopon-run` executable loads read-only Wasm providers and is documented in [`run.md`](run.md). Its flags and effects are not part of this catalog CLI contract.

## Commands

```text
dekopon version
dekopon get agents
dekopon get agent <NAME>
dekopon get capabilities
dekopon get capability <NAME>
dekopon get providers
dekopon get provider <NAME>
dekopon describe agent <NAME>
dekopon validate
dekopon config view
```

Run `dekopon --help` or `dekopon <COMMAND> --help` for generated syntax.

## Global flags

- `--config <PATH>`: authoritative YAML or JSON source.
- `-o, --output <FORMAT>`: `table` (default), `wide`, `json`, `yaml`, or `name`.
- `--no-color`: disable ANSI color in diagnostics.
- `--quiet`: suppress successful output; errors still print.
- `-v`: emit informational diagnostics and error causes.
- `-vv`: emit debug diagnostics and debug error context.

Global flags may appear before or after subcommands.

## Configuration discovery

Paths are considered in this exact order:

1. `--config <PATH>`
2. `DEKOPON_CONFIG`
3. `$XDG_CONFIG_HOME/dekopon/config.yaml`
4. `$HOME/.config/dekopon/config.yaml`
5. `./dekopon.yaml`

An explicit or environment path is authoritative: if it cannot be read, the command fails rather than falling back. For default locations, the first existing regular file wins. An empty environment variable is ignored. If no default exists, the error lists all searched paths.

The loader accepts JSON, a single YAML resource, a YAML sequence, or multiple YAML documents. It parses the file once and rejects unknown fields, malformed IDs, duplicate names, missing capability references, and missing provider references.

## Output behavior

List output is sorted by validated identifier. `name` output uses qualified names such as `agent/reviewer`. A singular JSON or YAML command emits one resource; a list emits a versioned `AgentList`, `CapabilityList`, or `ProviderList`. `config view` emits a canonical grouped catalog rather than preserving comments or original document order.

## Exit codes

| Code | Meaning |
|---:|---|
| `0` | Success |
| `1` | Configuration, validation, rendering, or general runtime failure |
| `2` | Command-line usage error (emitted by Clap) |
| `3` | Requested resource not found |

These codes are stable for the `0.1.x` CLI.

## Examples

```console
dekopon --config examples/local/dekopon.yaml get agents
dekopon --config examples/local/dekopon.yaml get agents -o wide
dekopon --config examples/local/dekopon.yaml get agent reviewer -o yaml
dekopon --config examples/local/dekopon.yaml get capabilities -o name
dekopon --config examples/local/dekopon.yaml describe agent reviewer
dekopon --config examples/local/dekopon.yaml validate
dekopon --config examples/local/dekopon.yaml config view --output json
```
