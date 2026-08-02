# dekopon-shell

A sandboxed, bash-flavored scripting language whose commands dispatch to [Dekopon](https://github.com/dekopon-agents/dekopon) capabilities instead of operating-system processes.

Exposing one model-facing tool schema per capability bloats a system prompt and forces a model into many small round trips. One scripting tool lets a model express a multi-step plan — loops, conditionals, functions, JSON handling — in a single tool call.

This crate is a pure interpreter library. It links no Wasmtime, no broker, no HTTP client, and no filesystem access. Everything a script can reach outside its own value space goes through one seam:

```rust
pub trait CapabilityInvoker {
    fn granted(&self) -> Vec<String>;
    fn is_granted(&self, capability: &str) -> bool { /* ... */ }
    fn describe(&self, capability: &str) -> Option<CapabilityDescription> { /* ... */ }
    fn invoke(&self, capability: &str, input: serde_json::Value) -> CapabilityCallResult;
}
```

## Value model

Every variable is a `serde_json::Value`, not bash text. Capability inputs and outputs therefore need no marshaling, and JSON, arrays, maps, and arithmetic are native rather than emulated. `|` hands one structured value to the next command, closer to `jq`'s own `|` than to byte-stream piping.

## Grammar

**Kept**: simple commands; `;`, `&&`, `||`, `|`; `#` comments; `if`/`elif`/`else`; `for`; `while`; `until`; `break`/`continue` with levels; functions with `$1`/`$@`/`$#` and `local` under bash's dynamic scoping; `$NAME`, `${NAME}`, `${NAME[index]}`; both quoting forms, bash-exact; `$( )`; `$(( ))`; `$?`; `return`; `exit`; and `>`/`>>` into named in-memory buffers read back by `cat`.

**Dropped, each rejected explicitly rather than silently ignored**: globbing, brace and tilde expansion, POSIX IFS word splitting, job control (a trailing `&` is a hard parse error), subshells, `eval`, `exec`, `source`, `declare`, `export`, here-documents, process substitution, `case`, and bash's own sparse/associative array emulation. A model must never be able to believe something happened that did not.

## Builtins

`jq` (the real [jaq](https://github.com/01mf02/jaq) engine), `curl` (a flag parser that submits to a capability, never a socket), `grep`, `sed`, `cut`, `sort`, `uniq`, `wc`, `base64`, `xargs`, `echo`, `printf`, `test`/`[`, `true`, `false`, `sleep`, `cat`, and the `cap` escape hatch. Every builtin name is separator-free, and capability fallback fires only for separator-containing words, so the two namespaces are provably disjoint.

## Sandboxing

This is a native tree-walking evaluator, so there is no engine fuel meter to fall back on. Every bound is hand-built and configurable: a step budget, a recursion-depth cap, independent output byte and line ceilings with head-and-tail truncation, a cooperative wall-clock deadline, and a capability-invocation ceiling kept deliberately separate from the step budget. The variable namespace is seeded only from the script's own assignments; the host process environment is never read.

## License

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your option.
