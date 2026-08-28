# dekopon-shell

A sandboxed, bash-flavored scripting language whose commands dispatch to [Dekopon](https://github.com/dekopon-agents/dekopon) capabilities instead of operating-system processes.

Exposing one model-facing tool schema per capability bloats a system prompt and forces a model into many small round trips. One scripting tool lets a model express a multi-step plan — loops, conditionals, functions, JSON handling — in a single tool call.

This crate is a pure interpreter library. It links no Wasmtime, no broker, no HTTP client, and no filesystem access. Everything a script can reach outside its own value space goes through one seam:

```rust
pub trait CapabilityInvoker {
    fn granted(&self) -> Vec<String>;
    fn is_granted(&self, capability: &str) -> bool { /* ... */ }
    fn describe(&self, capability: &str) -> Option<CapabilityDescription> { /* ... */ }
    fn invoke(
        &self,
        capability: &str,
        input: serde_json::Value,
        secret_use: Option<SecretUseProposal>,
    ) -> CapabilityCallResult;
}
```

## Value model

Every variable is a `serde_json::Value`, not bash text. Capability inputs and outputs therefore need no marshaling, and JSON, arrays, maps, and arithmetic are native rather than emulated. `|` hands one structured value to the next command, closer to `jq`'s own `|` than to byte-stream piping.

A here-document lands in that model as a plain string: a block of literal text is what a string is here, and no byte stream is involved anywhere. A body that happens to look like JSON is *not* parsed for you — `jq . <<EOF` sees a string, and `jq 'fromjson' <<EOF` is how you cross over — because auto-parsing would make `cat <<EOF` mean two different things depending on its contents. The newline ending the last body line is dropped, matching the rule that values here are not newline-terminated (`echo hi` produces `"hi"`, and emitting a value is what adds the line ending), so `cat <<EOF` prints what bash prints rather than a trailing blank line.

## Grammar

**Kept**: simple commands and compound ones — `if`, `for`, `while`, `until`, `case`, and `{ ...; }` groups — anywhere a command may appear, including as a pipeline stage; `;`, `&&`, `||`, `|`; a leading `!` to invert a pipeline; `#` comments; `if`/`elif`/`else`; `for`; `while`; `until`; `case`/`esac`; `[[ ... ]]`; `break`/`continue` with levels; functions with `$1`/`$@`/`$*`/`$#`, `shift`, `getopts`, and `local` under bash's dynamic scoping; `read`; `$NAME`, `${NAME}`, `${NAME[index]}`, `${NAME[@]}`/`${NAME[*]}`, `${#NAME}`, and the substitution forms `${NAME:-w}`, `${NAME:=w}`, `${NAME:?w}`, `${NAME:+w}`, `${NAME#p}`, `${NAME%p}`, `${NAME/p/r}`; both quoting forms, bash-exact, including `"$@"` splitting one word per parameter; `$( )`; `$(( ))`; `$?`; `${PIPESTATUS[@]}`; `set -e`, `set -u`, `set -o pipefail` and their `+` forms; `return`; `exit`; here-documents `<<EOF`, `<<-EOF`, and the literal `<<'EOF'`; and redirection of either stream — `>`, `>>`, `2>`, `2>>`, `&>`, `&>>`, `2>&1`, `>&2` — into named in-memory buffers read back by `cat`.

**Dropped and rejected loudly** — the script fails to parse or run, naming the construct: backtick substitution (use `$( )`), job control (a trailing `&`), subshells, the arithmetic command `(( ))`, bash array literals `name=(a b c)`, C-style `for (( ))`, every `set` option this shell does not enforce, descriptors other than 1 and 2, here-strings (`<<<`), `case` fall-through (`;&`, `;;&`), process substitution, `eval`, `exec`, `source`, `declare`, `export`, bash's sparse/associative array emulation, case-conversion and `@`-operator parameter expansions, regex metacharacters in an unflagged `grep`/`sed` pattern, and glob metacharacters in a `case` pattern. A model must never be able to believe something happened that did not.

That last one is where "rejected loudly" reaches inside a construct that was kept. A `case` pattern is matched as literal text, so `*)` remains the default branch — every subject reaches it, which is what a literal matcher concludes too — while `*.json)`, `a?c)`, and `[ab])` are parse errors naming the metacharacter and what it would have meant. This is the same rule `grep` and `sed` patterns follow when they are given no `-E`, for the same reason: a partial wildcard is exactly the pattern a literal matcher answers wrongly and silently. Quoting stays the escape hatch, so `'*')` matches a literal asterisk. A pattern assembled at run time (`p='*.json'; case $f in $p)`) is checked when it is expanded rather than when it is parsed, because that is the first moment its text exists.

**Dropped and inert** — these are ordinary literal text, and a script cannot tell the difference: globbing (`*`, `?`, `[abc]`), brace expansion (`{a,b}`), tilde expansion (`~`), and POSIX IFS word splitting. There is no filesystem to glob against and no `IFS` to split on, so there is nothing to reject *against*; an unquoted expansion holding a JSON array is what produces multiple words here. This is the one place where the "rejected loudly" rule does not apply, and it is called out rather than folded into the list above.

## `read`

`read [-r] NAME...` is what makes `cmd | while read line; do ...; done` terminate, and it is the one
place input is *consumed*. Everywhere else a piped value is *offered*: every pipeline in a body sees
it, deliberately, so a condition that never looks at it cannot swallow it before the command that
does. `read` instead advances a cursor on the enclosing stage and reports failure at end of input,
which is what ends the loop. End of input is a status and not a diagnostic, because a message there
would be one per loop, every loop.

Several names split the line on whitespace runs with the remainder in the last, exactly as bash
does. That is a rule local to `read`, not a return of POSIX IFS word splitting: nothing else here
splits, and there is no `IFS` to configure. `-r` is accepted and changes nothing — backslashes are
never line continuations here — which is said out loud rather than left as a surprise.

`getopts OPTSTRING NAME` parses a shell function's own flags, setting `OPTIND` and `OPTARG` as
ordinary variables. It is scoped to a function because that is the only place positional parameters
exist here, and says so rather than silently finding none.

## Shell options

`set -e`, `set -u`, and `set -o pipefail` are real, and their `+` forms turn them back off. `set`
was refused outright before on the grounds that an option changing nothing while looking like it had
is the exact class of silent wrongness this shell refuses — which stops being an argument once the
option is enforced, and remains one for every option that is not. So `set -x`, `set -o noclobber`,
and `set --` **end the script** by name rather than being ignored or letting it carry on without
what it asked for.

`errexit` exempts the three positions bash exempts, because each is a place the script is already
asking whether the command failed: an `if`/`while`/`until` condition, every operand of an `&&`/`||`
chain but the last, and a pipeline inverted by `!`. They compose — `if a && b; then` nests two — so
the exemption is a counter rather than a flag.

`pipefail` makes a pipeline report its rightmost failing stage. It matters more here than in bash:
`some.capability x | jq .` succeeds by default even when the capability never ran, because `jq` was
handed nothing and had no complaint. `${PIPESTATUS[@]}` is an ordinary global holding one status per
stage, so `${PIPESTATUS[0]}` and `${#PIPESTATUS[@]}` work through the expansion machinery already
here.

`nounset` governs a plain `$name` only. `${name:-default}` and its relatives exist precisely to
handle an absent value, so tripping on them would make the option refuse the idiom written to
satisfy it.

## `[[ ... ]]`

`[[ ... ]]` runs the **same tests** `[` and `test` run — one function, so the two spellings cannot
disagree about what `-z` or `-lt` mean, and file tests stay refused with the same message. What it
adds is the connective grammar bash gives it (`&&`, `||`, `!`, parentheses, all short-circuiting)
and the promise that an unquoted expansion is one operand, so `[[ -n $v ]]` holds for a value with
spaces where `[ -n $v ]` falls apart.

One thing is refused rather than translated. In bash the right operand of `==` is a **glob**, so
`[[ $f == *.json ]]` is a pattern match; the operand here is literal text, and comparing that
operand literally would answer exactly that script wrongly and silently. The metacharacter is named
instead, with quoting as the way out while the parser can still see it (`[[ $f == '*' ]]` compares
an asterisk) and a run-time-assembled operand checked when it expands. `=~` is refused outright for
a related reason: `[[ ]]` carries no `-E`, so nothing inside it can say a regular expression was
what the script meant. `grep -E` is where that opt-in lives.

`<` and `>` inside the brackets are string comparisons, not redirections — the lexer cannot know
that, so the parser translates them back.

## Compound commands

`if`, `for`, `while`, `until`, `case`, and `{ ...; }` are pipeline stages, not just statements, so
`cmd | while ...; do ...; done` and `cmd || { echo failed; exit 1; }` are ordinary things to write.
They parse through the same production either way, and carry their own redirections:
`{ a; b; } 2> log`.

A compound stage runs in the **current scope**. There are no subshells here to run it in, so a
variable a piped `while` loop assigns is still set after the loop — the opposite of bash, where that
assignment is thrown away with the subshell and is the single most notorious trap in the language.
The obvious script does the obvious thing.

A stage feeding a pipe, a redirection, or a `$( )` has its emissions collected into one value, since
each statement inside emits separately and `{ echo a; echo b; } | wc -l` must see both. That is the
same collection a command substitution already performed.

`{ ...; }` is a group, not a subshell, and it is spelled out as such: an empty `{ }` and an
unterminated `{ echo hi` are parse errors naming themselves rather than quietly running nothing.

## Parameter expansion

`${NAME:-w}`, `${NAME:=w}`, `${NAME:?w}`, `${NAME:+w}` and their colon-free forms behave as bash
does, including the distinction the colon draws: `:-` substitutes for a name holding nothing, `-`
only for one nothing ever assigned. `${NAME:?w}` **ends the script**, because that is what the
construct is for — reporting a status and carrying on with an empty string would leave a script
believing it had the value it just asserted it needed.

Two of them answer differently here than in bash, because values are real JSON rather than text.
`${#NAME}` counts characters of a string, but *elements* of an array and *keys* of an object; the
character count of an object's JSON text would be an answer about its rendering rather than about
the value. And `${NAME[@]}` is not bash's sparse-array emulation: it selects the elements of a real
JSON array, so `"${arr[@]}"` yields one word per element the way `"$@"` does, `${arr[*]}` joins
them, and `${#arr[@]}` counts them. An unquoted `$NAME` holding an array already spread element by
element; `[@]` is how that survives quoting.

`${NAME#p}`, `${NAME%p}`, and `${NAME/p/r}` take **literal** patterns, the rule an unflagged
`grep`, an unflagged `sed`, and a `case` pattern already follow. A literal pattern matches in
exactly one way, so bash's longest/shortest pairs (`##`, `%%`) are accepted as a second spelling of
the same request rather than as a second behavior. A metacharacter is rejected by name: `${p##*/}` is a parse error, and
quoting is the way through (`${p#'*'}` strips a literal asterisk) exactly while the parser can still
see the quotes. A pattern assembled at run time is checked when it expands instead, and quoting
cannot exempt that one because its quotes are already gone. `basename` and `dirname` are the answer
for the path-slicing idiom `${p##*/}` would have covered.

A whole right-hand side keeps its value rather than collapsing to text — `copy=$obj` followed by
`${copy[key]}` works, the same deviation that already made `ip=$(curl ...)` followed by
`${ip[origin]}` work. Glued to anything else it is text again.

Reading a `${NAME:-...}` substitute or a `${NAME[...]}` index re-enters the tokenizer on the native
stack, so nesting has a fixed ceiling and deep `${a:-${a:- ... }}` is a lex error rather than a
crashed host process — the same bound the parser applies to `$( $( ... ) )`.

## The two streams

A command produces a **value** on stdout and **text** on stderr, and that split was always there:
`$( )` captures the value while diagnostics escape it to the terminal, exactly as a real shell
sends command-substitution stderr past the capture. What is new is that a script can address the
two halves. `2> log` collects a command's diagnostics into a buffer; `>&2` sends its value to the
diagnostic stream, which is how `echo "problem" >&2` reports without polluting what the command
returns; `&> all` sends both to one buffer; and `> /dev/null` discards, that one name being reserved
rather than a path, because it is the spelling every shell shares and refusing it would be worse
than admitting it.

`2>&1` is the one place the value model shows through. Merging a text channel into a value channel
has to mean something exact, so it means what every text-shaped builtin already means: the
diagnostics become extra lines. A command that produced no diagnostics has nothing to merge and its
value is left alone — including its type, so `posts.get 2>&1` still hands `jq` an object rather than
that object's JSON text. The result is that `x=$(cmd 2>&1)` captures *why* something failed, which
is the idiom the construct exists for.

Redirections resolve left to right, so a duplication copies the destination its target holds at that
point. The reversed spelling `2>&1 > buf` is a parse error naming itself: bash copies the file
*description* there and leaves stderr pointing at the terminal, this interpreter has destinations
rather than descriptions and cannot represent the difference, and that ordering is precisely the one
a script writes when it believes it captured diagnostics that went somewhere else.

`ScriptOutcome::output` stays one combined, interleaved stream. The streams are addressable from
inside a script; what a caller receives is still the transcript a terminal would have shown.

## Builtins

`jq` (the real [jaq](https://github.com/01mf02/jaq) engine), `curl` (a flag parser that submits to a capability, never a socket), `date`, `grep`, `sed`, `cut`, `sort`, `uniq`, `wc`, `base64`, `xargs`, `echo`, `printf`, `test`/`[`, `true`, `false`, `sleep`, `cat`, and the `cap` escape hatch. Every builtin name is separator-free, and capability fallback fires only for separator-containing words, so the two namespaces are provably disjoint.

`grep` and `sed` are the only two that take `-E`, and it is the only way regex syntax ever becomes regex syntax here. Unflagged, both read a literal pattern and reject an unescaped metacharacter by name — one matching semantics shared with `case`, `${p#…}`, and the right operand of `[[ == ]]`, so a script never has to know which construct it is in to know what `[0-9]` means. With `-E` the pattern goes to `regex-bites`, the engine `jq`'s own `regex` builtins already link, rather than to a second one added for this. Anchors are real (`grep -E '^ba(r|z)$'`, `sed -E 's/^ *//'`), `.` is the wildcard, and the engine's compile error is reported by name when a pattern does not compile. An `-E` pattern is model-authored text, so it is bounded before it sees input: 1 KiB of pattern source, a 64 KiB compiled program, and sixteen levels of nesting. A replacement stays literal text in both modes — the engine's `$1` interpolation is off, and a real-sed `\1` group reference is refused rather than emitted verbatim. `-iE` folds ASCII case only, because this engine matches codepoint by codepoint; that is narrower than the literal path's `-i`, never wider.

Two of them exist only when the embedder configured them, and are "command not found" otherwise: `curl`, whose target capability is fixed for the whole execution, and `date`, which is gated by the off-by-default `Limits::allow_clock`. A broker-backed `curl` may additionally propose an inert public DRN through exact `--oauth2-bearer '${drn:…}'` or `-u 'USER:${drn:…}'` syntax. The DRN stays in a typed invocation field and is removed before provider JSON is built. Literal passwords and arbitrary interpolation are refused; direct invokers deny the proposal. The broker separately authorizes and resolves it as documented in [`docs/secrets.md`](../../docs/secrets.md).

Reading the wall clock is ambient authority with no capability to go through — there is no provider for "what time is it", and minting a fake one would be a capability nobody granted and nothing audited — so it is an explicit embedder opt-in instead. Off, `date` looks exactly like any ungranted capability; a script cannot distinguish "not permitted" from "not a command" and go looking for a way around. On, it renders the current UTC time as `+%s` or an ISO-8601 instant, and rejects every other format by name rather than shipping a partial `strftime` whose gaps would each be a silently wrong answer.

## Sandboxing

This is a native tree-walking evaluator, so there is no engine fuel meter to fall back on. Every bound is hand-built and configurable: a step budget, a recursion-depth cap, independent output byte and line ceilings with head-and-tail truncation, a wall-clock deadline re-read on every step and around every capability call, a capability-invocation ceiling kept deliberately separate from the step budget, and a cumulative ceiling on the value bytes a script may materialize — the one that bounds a script which is cheap in steps and expensive in memory, such as doubling a string in a loop.

Parsing has its own fixed nesting ceiling, applied before any budget exists, because the parser is recursive and runs on the native stack: deeply nested `$( $( ... ) )` is a syntax error rather than a stack overflow that would abort the host process without producing an outcome at all.

The variable namespace is seeded only from the script's own assignments; the host process environment is never read. That includes `jq`: jaq's standard library exports an `env` filter reading the real process environment, and it is deliberately not linked, along with `now`.

One residual is worth stating plainly rather than leaving to be discovered. jaq has no fuel meter and no safe point to interrupt from outside, so a filter that loops without producing output (`jq 'def f: f; f'`) cannot be stopped cooperatively. It runs on a worker thread whose outputs are charged against the budget as they arrive, and a filter still running at the deadline is abandoned rather than stopped: the script reports its timeout correctly, and that thread stays alive until the process exits.

Dropping the receiver is the cancellation check for every filter that *does* yield — it fails its next send and returns — so what accumulates is only the non-terminating kind. Each abandonment logs a `tracing::warn!` with the elapsed time, `dekopon_shell::abandoned_filter_workers()` reports how many are still running, and past a small ceiling `jq` refuses to start another filter rather than adding one more spinning thread to a host that is already saturated.

The worker belongs to the thread, not to the filter: a thread that has run one filter parks its worker on a job channel and hands it the next, so a script full of `curl ... | jq ...` pays one thread rather than one per call. Abandoning a filter also gives up that worker, so the filter nobody can stop is never offered another one and the next `jq` starts from a freshly spawned worker. Values cross the boundary as values — jaq's own type deserializes from `serde_json::Value` and converts back structurally — rather than being rendered to JSON text and re-parsed on each side. jaq's value type is a JSON superset, so the outputs JSON cannot represent (`nan`, `infinite`, byte strings, non-string object keys) are refused by name, as the JSON parser refused them before, and output nesting keeps the same 128-container ceiling that parser applied.

## Observability

Each script run opens a `tracing` span named `shell.script`, and every command word inside it opens
a `shell.command` span — the span carries the whole record, and there are no events. A trace
therefore reads as the ordered list of commands a script actually executed — `jq`, then `curl`, then
`http-probe.fetch`, then `grep` — rather than as one opaque "a script ran, exit 0". One script word
that drives several executions is shown as several: `xargs` mapping a command over ten items
produces ten nested spans.

Volume is capped rather than detail. A model-authored `while` loop is bounded only by the step
budget, so one tool call can execute tens of thousands of command words; the first few hundred
`shell.command` spans are emitted at INFO and the rest at DEBUG. The `shell.script` span carries the
run's totals — commands executed, commands traced, capability commands, failed commands — which
cost the same whether a script ran three commands or thirty thousand.

A word that resolved to nothing is reported as `not-granted` when it names a capability in a
namespace this session holds, and `not-found` otherwise. Only the namespace is exported, from the
session's own granted set; the word stays `<withheld>` unless payloads are enabled. The script sees
identical output either way — the distinction is in the span and nowhere a script can read it.

Instrumentation lives at the single seam every command word passes through, so a builtin added
later is traced without another edit, and none of the twenty builtin implementations carries
telemetry code.

`tracing` is this crate's only dependency for that. There is no exporter here, no collector, and no
telemetry protocol — the embedding binary's subscriber decides where spans go, exactly as `curl`
here links no HTTP client and only assembles a request for one capability. Spans must therefore be
assumed to leave the process, so a command records its name, its resolution kind, its argument
*count*, a duration, an exit code, and a stable outcome label, and never an argument value: a
`curl -d` body and a `cap <id> {...}` object are capability input wearing argv's clothes. A
model-authored command word — a shell function's name, or a word that resolved to nothing — is
reported as `<withheld>` rather than copied, while its kind still says what happened.

## License

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your option.
