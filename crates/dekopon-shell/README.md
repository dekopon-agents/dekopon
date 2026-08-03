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

**Kept**: simple commands; `;`, `&&`, `||`, `|`; a leading `!` to invert a pipeline; `#` comments; `if`/`elif`/`else`; `for`; `while`; `until`; `break`/`continue` with levels; functions with `$1`/`$@`/`$*`/`$#`, `shift`, and `local` under bash's dynamic scoping; `$NAME`, `${NAME}`, `${NAME[index]}`; both quoting forms, bash-exact, including `"$@"` splitting one word per parameter; `$( )`; `$(( ))`; `$?`; `return`; `exit`; and `>`/`>>` into named in-memory buffers read back by `cat`.

**Dropped and rejected loudly** — the script fails to parse or run, naming the construct: backtick substitution (use `$( )`), job control (a trailing `&`), subshells, the arithmetic command `(( ))`, bash array literals `name=(a b c)`, C-style `for (( ))`, `[[ ]]`, `set` and its options, file-descriptor redirection (`2>`, `>&2`, `2>&1`), here-documents, process substitution, `case`, `eval`, `exec`, `source`, `declare`, `export`, bash's sparse/associative array emulation, `${name:-default}`-style parameter expansions, and regex metacharacters in a `grep`/`sed` pattern. A model must never be able to believe something happened that did not.

**Dropped and inert** — these are ordinary literal text, and a script cannot tell the difference: globbing (`*`, `?`, `[abc]`), brace expansion (`{a,b}`), tilde expansion (`~`), and POSIX IFS word splitting. There is no filesystem to glob against and no `IFS` to split on, so there is nothing to reject *against*; an unquoted expansion holding a JSON array is what produces multiple words here. This is the one place where the "rejected loudly" rule does not apply, and it is called out rather than folded into the list above.

## Builtins

`jq` (the real [jaq](https://github.com/01mf02/jaq) engine), `curl` (a flag parser that submits to a capability, never a socket), `grep`, `sed`, `cut`, `sort`, `uniq`, `wc`, `base64`, `xargs`, `echo`, `printf`, `test`/`[`, `true`, `false`, `sleep`, `cat`, and the `cap` escape hatch. Every builtin name is separator-free, and capability fallback fires only for separator-containing words, so the two namespaces are provably disjoint.

## Sandboxing

This is a native tree-walking evaluator, so there is no engine fuel meter to fall back on. Every bound is hand-built and configurable: a step budget, a recursion-depth cap, independent output byte and line ceilings with head-and-tail truncation, a wall-clock deadline re-read on every step and around every capability call, a capability-invocation ceiling kept deliberately separate from the step budget, and a cumulative ceiling on the value bytes a script may materialize — the one that bounds a script which is cheap in steps and expensive in memory, such as doubling a string in a loop.

Parsing has its own fixed nesting ceiling, applied before any budget exists, because the parser is recursive and runs on the native stack: deeply nested `$( $( ... ) )` is a syntax error rather than a stack overflow that would abort the host process without producing an outcome at all.

The variable namespace is seeded only from the script's own assignments; the host process environment is never read. That includes `jq`: jaq's standard library exports an `env` filter reading the real process environment, and it is deliberately not linked, along with `now`.

One residual is worth stating plainly rather than leaving to be discovered. jaq has no fuel meter and no safe point to interrupt from outside, so a filter that loops without producing output (`jq 'def f: f; f'`) cannot be stopped cooperatively. It runs on a worker thread whose outputs are charged against the budget as they arrive, and a filter still running at the deadline is abandoned rather than stopped: the script reports its timeout correctly, and that thread stays alive until the process exits.

## License

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your option.
