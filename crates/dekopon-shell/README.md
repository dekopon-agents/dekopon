# dekopon-shell

A sandboxed, bash-flavored scripting language whose commands dispatch to [Dekopon](https://github.com/dekopon-agents/dekopon) capabilities instead of operating-system processes.

Exposing one model-facing tool schema per capability bloats a system prompt and forces a model into many small round trips. One scripting tool lets a model express a multi-step plan — loops, conditionals, functions, JSON handling — in a single tool call.

This crate is a pure interpreter library. It links no Wasmtime, no broker, no HTTP client, and no filesystem access. Everything a script can reach outside its own value space goes through one seam:

```rust
pub trait CapabilityInvoker: Send + Sync {
    fn granted(&self) -> Vec<String>;
    fn is_granted(&self, capability: &str) -> bool { /* scans `granted` */ }
    fn command_words(&self) -> Vec<String> { /* none */ }
    fn has_command_word(&self, word: &str) -> bool { /* scans `command_words` */ }
    fn run_command(
        &self,
        word: &str,
        argv: &[String],
        stdin: Option<&str>,
    ) -> Option<CommandRun> { /* None: no provider owns the word */ }
    fn describe(&self, capability: &str) -> Option<CapabilityDescription> { /* None */ }
    fn invoke(&self, proposal: CommandProposal) -> CapabilityCallResult;
    fn script_finished(&self) { /* no-op: the script's last command has returned */ }
}
```

## Command resolution

A command word resolves in a fixed order. A word this shell refuses outright (`eval`, `exec`, `source`, job control, `declare`) ends the script naming itself; otherwise a shell function declared earlier in the script runs, then a builtin, then a command word the session's loaded providers contribute. Anything else is `command not found` at exit `127`, and that includes a word shaped like a capability identifier, granted or not: `wikipedia_page --title x` is an unknown command. A capability is reached only through the provider command word that proposes it. `cap --list` prints what this session was granted, `cap --describe <id>` prints one identifier and its description, and `<word> --help` is how to use it.

## Provider command words

A loaded provider can contribute bare words — `gh pr view 12` — and each behaves like its own command-line program run through `run_command`. `<word> --help` renders on stdout at whatever status the provider chose (`0` for help, `2` for a usage error by `clap` convention), so `h=$(gh --help)` captures the page like any other value. A `CommandRun::Rendered` answer charges no capability call; its bytes are charged against the value ceiling and both streams then obey the output ceilings, and its stderr goes to the diagnostic stream, so it escapes a `$( )` capture unless the script says `2>&1`. A `CommandRun::Failed` answer is a usage error at exit `2`. A run that never reached the provider's answer is not: `CommandRun::Errored` (the broker was unreachable, the host refused the input or trapped, the task did not complete) is reported like a capability that ran and errored, at exit `1`, and `CommandRun::Denied` (the session was cancelled underneath the run) like a refused capability, at exit `126`, so the model reads them as "retry later" or "stop" rather than "fix your argv". A `CommandRun::Proposed` answer is invoked through the same budget, denial, and telemetry path as every capability call, and one naming a capability this session was not granted exits `127` naming it, because the provider proposes without knowing what was granted. The proposal's owned report follows it into `invoke`; dropping a refused proposal settles that report as failed, without a shared pending slot. A proposal may also carry `secret_use`, the typed intent to use one public DRN the provider's command named. It reaches `CapabilityInvoker::invoke` unchanged, the broker authorizes it separately as documented in [`docs/secrets.md`](../../docs/secrets.md), and an invoker with no broker behind it refuses it.

A capability that ran and failed reports `<id>: failed: <classification>` at exit `1`, and when
the broker's classification came from the provider's own typed failure its code and message follow:
`gpt-image.edit: failed: provider-failure: upstream-rejected: the image route refused the request
with HTTP 400 (moderation_blocked)`. The classification is what the exit status means; the tail is
the provider's sentence, and it is how an upstream refusal reaches the model at all rather than
being guessed at.

A provider receives its own pipe's bytes as UTF-8 text; invalid UTF-8 fails that stage rather than being replaced. `echo hello | gh issue create -` supplies `hello\n`, while a provider without its own pipe or here-document receives `None`.

## Value model

Variables are `serde_json::Value`; pipeline stages exchange bytes, not values. `echo` appends a newline, `printf` does not, and a here-document supplies its literal text. `jq` parses each input JSON document, including scalars, with a depth limit; invalid input exits with status 2. It emits each filter result as a separate compact JSON line, quoting strings unless `-r` is set; `-c` changes nothing. An empty result emits no document. `-n` runs once on null without reading stdin; `-s` retains the document array against the value-byte budget and runs once, including on empty input (`[]`). Its worker compiles the filter once per stage and emits the first result without waiting for the end of stdin. A downstream stage that closes early kills and joins the worker.

## Grammar

**Kept**: simple commands and compound ones — `if`, `for`, `while`, `until`, `case`, and `{ ...; }` groups — anywhere a command may appear, including as a pipeline stage; `;`, `&&`, `||`, `|`; a leading `!` to invert a pipeline; `#` comments; `if`/`elif`/`else`; `for`; `while`; `until`; `case`/`esac`; `[[ ... ]]`; `break`/`continue` with levels; functions with `$1`/`$@`/`$*`/`$#`, `shift`, and `local` under bash's dynamic scoping; `unset`; the no-op `:`; `read`; `$NAME`, `${NAME}`, `${NAME[index]}`, `${NAME[@]}`/`${NAME[*]}`, `${#NAME}`, and the substitution forms `${NAME:-w}`, `${NAME:=w}`, `${NAME:?w}`, `${NAME:+w}`, `${NAME#p}`, `${NAME%p}`, `${NAME/p/r}`; both quoting forms, bash-exact, including `"$@"` splitting one word per parameter; `$( )`; `$(( ))`; `$?`; `${PIPESTATUS[@]}`; `set -e`, `set -u`, `set -o pipefail` and their `+` forms; `return`; `exit`; here-documents `<<EOF`, `<<-EOF`, and the literal `<<'EOF'`; and redirection of either stream — `>`, `>>`, `2>`, `2>>`, `&>`, `&>>`, `2>&1`, `>&2` — into named in-memory buffers read back by `cat`.

**Dropped and rejected loudly** — the script fails to parse or run, naming the construct: backtick substitution (use `$( )`), subshells, the arithmetic command `(( ))`, bash array literals `name=(a b c)`, C-style `for (( ))`, every `set` option this shell does not enforce, descriptors other than 1 and 2, here-strings (`<<<`), `case` fall-through (`;&`, `;;&`), process substitution, `eval`, `exec`, `source`, `declare`, `export`, bash's sparse/associative array emulation, case-conversion and `@`-operator parameter expansions, regex metacharacters in an unflagged `grep`/`sed` pattern, and glob metacharacters in a `case` pattern. A model must never be able to believe something happened that did not.

"Rejected loudly" reaches inside `case` too. A `case` pattern is matched as literal text, so `*)` remains the default branch — every subject reaches it, which is what a literal matcher concludes too — while `*.json)`, `a?c)`, and `[ab])` are parse errors naming the metacharacter and what it would have meant. This is the same rule `grep` and `sed` patterns follow when they are given no `-E`, for the same reason: a partial wildcard is exactly the pattern a literal matcher answers wrongly and silently. Quoting stays the escape hatch, so `'*')` matches a literal asterisk. A pattern assembled at run time (`p='*.json'; case $f in $p)`) is checked when it is expanded rather than when it is parsed, because that is the first moment its text exists.

**Dropped and inert** — these are ordinary literal text, and a script cannot tell the difference: globbing (`*`, `?`, `[abc]`), brace expansion (`{a,b}`), tilde expansion (`~`), and POSIX IFS word splitting. There is no filesystem to glob against and no `IFS` to split on, so there is nothing to reject *against*; an unquoted expansion holding a JSON array is what produces multiple words here. This is the one place where the "rejected loudly" rule does not apply, and it is called out rather than folded into the list above.

## `read`

`read [-r] NAME...` is what makes `cmd | while read line; do ...; done` terminate, and it is the one
place input is *consumed*. A compound stage owns one input stream: a command that does not read it
leaves it for the next stdin-reading builtin in that stage. `read` advances the enclosing stream
and reports failure at end of input,
which is what ends the loop. End of input is a status and not a diagnostic, because a message there
would be one per loop, every loop.

Several names split the line on whitespace runs with the remainder in the last, exactly as bash
does. That is a rule local to `read`, not a return of POSIX IFS word splitting: nothing else here
splits, and there is no `IFS` to configure. `-r` is accepted and changes nothing — backslashes are
never line continuations here — which is said out loud rather than left as a surprise.

There is no `getopts`. Nothing calls a function in this shell with flags: a model writes both the
caller and the callee, and it writes `f "$x" "$y"`. A flag parser for the one caller that already
knows the argument order is a bash habit rather than a need, so a function reads `$1`, `$@`, `$#`,
and `shift` and stops there.

## Shell options

`set -e`, `set -u`, and `set -o pipefail` are real, and their `+` forms turn them back off. An
option that changes nothing while looking like it had is the exact class of silent wrongness this
shell refuses, so `set -x`, `set -o noclobber`, and `set --` **end the script** by name rather than
being ignored or letting it carry on without what it asked for.

`errexit` exempts the three positions bash exempts, because each is a place the script is already
asking whether the command failed: an `if`/`while`/`until` condition, every operand of an `&&`/`||`
chain but the last, and a pipeline inverted by `!`. They compose — `if a && b; then` nests two — so
the exemption is a counter rather than a flag.

`pipefail` makes a pipeline report its rightmost failing stage. It matters more here than in bash:
`gh issue view 12 | jq .` succeeds by default even when the capability never ran, because `jq` was
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
instead, with quoting as the way out while the parser can see it (`[[ $f == '*' ]]` compares
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

The last stage runs in the current scope, retaining assignments. Every earlier stage, including a
compound, function, or `xargs`, runs on its own thread from a scope snapshot: assignments and
named-buffer redirects there do not change the parent. `exit`, `return`, and `break` in a non-final
stage end only that stage. Inside a compound, `read`, `cat`, and stdin-reading builtins share its
one-shot input stream; provider commands do not inherit that stream.

Pipeline stages exchange bytes; `xargs` runs one input line at a time and emits each command's
output before reading the next line. Named redirection buffers append exact bytes, including invalid
UTF-8, and `cat` copies those bytes to another pipe or buffer. Expanding buffer bytes into text or
passing them to a provider requires valid UTF-8. A command substitution strips trailing LF bytes;
a whole assignment from JSON object or array text retains its structure, while unquoted text
substitutions split on newlines rather than spaces.

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
them, and `${#arr[@]}` counts them. An unquoted `$NAME` holding an array spreads element by
element; `[@]` is how that survives quoting.

`${NAME#p}`, `${NAME%p}`, and `${NAME/p/r}` take **literal** patterns, the rule an unflagged
`grep`, an unflagged `sed`, and a `case` pattern already follow. A literal pattern matches in
exactly one way, so bash's longest/shortest pairs (`##`, `%%`) are accepted as a second spelling of
the same request rather than as a second behavior. A metacharacter is rejected by name: `${p##*/}` is a parse error, and
quoting is the way through (`${p#'*'}` strips a literal asterisk) exactly while the parser can see
the quotes. A pattern assembled at run time is checked when it expands instead, and quoting
cannot exempt that one because its quotes are already gone. There is no `basename` or `dirname`;
`jq -r 'split("/") | last'` covers what `${p##*/}` would have.

A whole right-hand side keeps its value rather than collapsing to text — `copy=$obj` followed by
`${copy[key]}` works, the same deviation that already made `issue=$(gh issue view 12)` followed by
`${issue[title]}` work. Glued to anything else it is text again.

Reading a `${NAME:-...}` substitute or a `${NAME[...]}` index re-enters the tokenizer on the native
stack, so nesting has a fixed ceiling and deep `${a:-${a:- ... }}` is a lex error rather than a
crashed host process — the same bound the parser applies to `$( $( ... ) )`.

## The two streams

A command produces a **value** on stdout and **text** on stderr: `$( )` captures the value while
diagnostics escape it to the terminal, exactly as a real shell sends command-substitution stderr
past the capture. A script addresses the two halves separately. `2> log` collects a command's diagnostics into a buffer; `>&2` sends its value to the
diagnostic stream, which is how `echo "problem" >&2` reports without polluting what the command
returns; `&> all` sends both to one buffer; and `> /dev/null` discards, that one name being reserved
rather than a path, because it is the spelling every shell shares and refusing it would be worse
than admitting it.

`2>&1` is the one place the value model shows through. Merging a text channel into a value channel
has to mean something exact, so it means what every text-shaped builtin already means: the
diagnostics become extra lines. A command that produced no diagnostics has nothing to merge and its
value is left alone — including its type, so `gh issue view 12 2>&1` hands `jq` an object rather
than that object's JSON text. The result is that `x=$(cmd 2>&1)` captures *why* something failed, which
is the idiom the construct exists for.

Redirections resolve left to right, so a duplication copies the destination its target holds at that
point. The reversed spelling `2>&1 > buf` is a parse error naming itself: bash copies the file
*description* there and leaves stderr pointing at the terminal, this interpreter has destinations
rather than descriptions and cannot represent the difference, and that ordering is precisely the one
a script writes when it believes it captured diagnostics that went somewhere else.

`ScriptOutcome::output` is one combined, interleaved stream. The streams are addressable from
inside a script; what a caller receives is the transcript a terminal would have shown.

## Builtins

`jq` (the real [jaq](https://github.com/01mf02/jaq) engine), `grep`, `sed`, `cut`, `sort`, `uniq`, `wc`, `head`, `tail`, `base64`, `xargs`, `echo`, `printf`, `test`/`[`, `true`, `false`, `sleep`, `progress` (a note through `CapabilityInvoker::note`, a no-op without a sink), `cat`, and `cap` (what this session was granted). There is no `curl` builtin; HTTP is reached through a provider command word like any other capability. A loaded provider may also contribute *command words* (for example `gh`), resolved after functions and builtins through `CapabilityInvoker::run_command` into a capability proposal that takes the ordinary budget, denial, and telemetry path, rendered text at the provider's own exit status, or a usage error (see [Provider command words](#provider-command-words)); a provider word that collides with a builtin, another reserved word, or another provider's word is refused at load by `dekopon_core::command_word_conflicts`, never shadowed here.

`head` and `tail` select lines from standard input (10 by default): `-n N` or `-N` selects a count, and `tail -n +N` starts at line N. `head -n 0` closes input without reading. Neither accepts file operands, `-c`, or `-f`.

Every builtin answers `--help` on stdout at exit 0, naming the flags it accepts (`true`, `false`, `sleep`, `cat`, and `printf` take none, so theirs reads `no flags`); the interpreter intercepts `--help` before the builtin's own argument parsing ever runs, so it wins even over a call that is otherwise malformed, such as `[ --help` with no closing `]`. The same accepted-subset text `--help` prints is what `unsupported_flag` appends to a refusal: `grep: option not yet supported: -q (supported: -v -i -c -n -E)`.

`grep` and `sed` are the only two that take `-E`, and it is the only way regex syntax ever becomes regex syntax here. Unflagged, both read a literal pattern and reject an unescaped metacharacter by name — one matching semantics shared with `case`, `${p#…}`, and the right operand of `[[ == ]]`, so a script never has to know which construct it is in to know what `[0-9]` means. With `-E` the pattern goes to `regex-bites`, the engine `jq`'s own `regex` builtins already link, rather than to a second one added for this. Anchors are real (`grep -E '^ba(r|z)$'`, `sed -E 's/^ *//'`), `.` is the wildcard, and the engine's compile error is reported by name when a pattern does not compile. An `-E` pattern is model-authored text, so it is bounded before it sees input: 1 KiB of pattern source, a 64 KiB compiled program, and sixteen levels of nesting. A replacement stays literal text in both modes — the engine's `$1` interpolation is off, and a real-sed `\1` group reference is refused rather than emitted verbatim. Flags are matched whole, so the two are written separately: `-i -E` folds ASCII case only, because this engine matches codepoint by codepoint; that is narrower than the literal path's `-i`, never wider. The bundled `-iE` is "option not yet supported".

The shell has no clock. Reading the wall time from here would be ambient authority with no capability to go through, so `date` is not a builtin. It was an embedder opt-in that no embedder ever set, and without a provider that claims it a script asking for the time gets "command not found" like any other unknown word. The time is a provider capability instead, granted, authorized, and audited like any other: a provider holding a durable-files storage grant already reads `wall-time-ms`, and the broker host's `dekopon:clock/wall@1.1.0` import is the clock a provider reads with no other grant, inside an authorized `invoke`. A provider that claims `date` proposes its clock capability from `run-command` and reads the clock when the broker invokes it, as the in-tree [`clock-probe`](../../examples/providers/clock-probe/README.md) fixture does.

## Sandboxing

This is a native tree-walking evaluator, so there is no engine fuel meter to fall back on. Every bound is hand-built and configurable: a step budget, a recursion-depth cap, independent output byte and line ceilings with head-and-tail truncation, a wall-clock deadline re-read on every step and around every capability call, a capability-invocation ceiling kept separate from the step budget, and a shared ceiling on retained value bytes, refunded when their owners drop or replace them — the one that bounds a script which is cheap in steps and expensive in memory, such as doubling a string in a loop.

Parsing has its own fixed nesting ceiling, applied before any budget exists, because the parser is recursive and runs on the native stack: deeply nested `$( $( ... ) )` is a syntax error rather than a stack overflow that would abort the host process without producing an outcome at all.

The variable namespace is seeded only from the script's own assignments; the host process environment is never read. That includes `jq`: jaq's standard library exports an `env` filter reading the real process environment, and it is not linked, along with `now`.

Each `jq` stage runs in a worker executable supplied by the embedder. On Linux its address space is limited to its startup size plus 256 MiB; at the deadline or on cancel the worker is killed and waited on. A crashed worker fails the stage, and the script can continue. jaq's value type is a JSON superset, so outputs JSON cannot represent (`nan`, `infinite`, byte strings, non-string object keys) are refused, and output nesting keeps the JSON parser's 128-container ceiling.

## Observability

Each script run opens a `tracing` span named `shell.script`, and every command word inside it opens
a `shell.command` span — the span carries the whole record, and there are no events. A trace
therefore reads as the ordered list of commands a script executed — `jq`, then `gh`, then `grep` —
rather than as one opaque "a script ran, exit 0". One script word that drives several executions is
shown as several: `xargs` mapping a command over ten items produces ten nested spans.

Nothing is capped. A model-authored `while` loop is bounded only by the step budget, so one tool
call can execute tens of thousands of command words, and each of them gets its INFO span: an
attribute may be truncated, a span is never dropped ([goal 2](../../docs/design.md#constitution)).
The `shell.script` span carries the run's totals beside them — commands executed, capability
commands, failed commands — which cost the same whether a script ran three commands or thirty
thousand.

Instrumentation lives at the single seam every command word passes through, so a builtin added
later is traced without another edit, and none of the twenty builtin implementations carries
telemetry code.

`tracing` is this crate's only dependency for that. There is no exporter here, no collector, and no
telemetry protocol — the embedding binary's subscriber decides where spans go. Spans must therefore
be assumed to leave the process, and they carry the whole command: its word — whoever wrote it, a
model-authored function name included — its resolution kind (`control`, `function`, `builtin`,
`provider-command`, `compound` for a compound pipeline stage, `rejected`, or `not-found`), its argument count, a duration, an exit code, and a
stable outcome label, beside three payloads. A pipeline id and zero-based stage index identify
concurrent stages; input/output byte counts, elapsed nanoseconds, and `end` or `reader_gone`
record their stream outcome. Spawned command spans inherit the script span. `shell.command.arguments`
is the argv after the word as a JSON array, `shell.command.stdin` the drained input for a provider or a non-streaming
stdin-reading builtin when recorded (not the bytes passing through a streaming builtin), and `shell.command.output` the command's rendered result. Each payload passes through
`dekopon_core::bounded_attribute`, which cuts a value past its byte cap on a character boundary and
marks the cut, and carries a `.bytes` sibling with its full length, so a truncated attribute still
says how much there was ([goal 2](../../docs/design.md#constitution)). A secret reference in argv is
a public DRN, never the secret it names; only the broker resolves one.

## License

Licensed under either of [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT) at your option.
