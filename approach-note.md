# errors-depth approach for the Sol arm

Pinned steering first line: `# Steering for GPT executors`.

This is the approach deliverable only. Implementation and Rust builds wait for the coordinator-launched Astra note review. The final implementation commit will delete this file. Branch: `codex/errors-depth-sol-1`; no other arm is an input.

## Base and conflicts: readings I will follow

- The dispatch mentions a coordinator-stamped base without providing a full SHA. The initial cached checkout was `3a73431f`, before StageOut. After the steering-required fetch, I selected current main `faacd1b887416a998c5f48199c505e7bff817d71`, a descendant of the stated minimum `29dd5b6ece0b4fab146214faf55b23eee990a910`, and checked it out before editing. The parser, lexer, depth code, arithmetic AST, grammar tests and both locks are identical between these two commits. This base selection remains an explicitly reported assumption for the coordinator's note review.
- PLAN second half and IDEA-0042 still list sharing parameter depth across command substitutions as work; PLAN's redirect-status F1 and the brief say it already landed. The base has `Depth::substitution` carrying the lexer/block sum forward, `Depth::parameter` consuming that count, and `alternating_expansions_share_parse_depth_on_a_two_mib_stack`. Preserve them; no second counter or ceiling change.
- The study warns about recursive unary arithmetic. The base instead collects unary operators iteratively and calls `ArithExpr::unary`, whose `Height::next` returns `ArithTooDeep` at 64. Preserve that implementation. The 2 MiB grammar regression already covers long unary and binary chains.
- IDEA-0042's approximately -10% lexer/parser target and the study's broader cleanup describe the aggregate work, much of which already landed. This step follows the brief's roughly neutral typed-error target and records any growth above +150 production code-only lines. No unrelated redirect or parser redesign.
- The study identifies a run of spaces in a conditional refusal. The brief makes every sentence exact, so preserve those spaces and all other wording. Only nested-body line numbers may change; retain exit 2.
- The study's one-hour fuzz exercise and 10 ms kill criterion concern a possible Pest replacement. This brief requires a stable ordinary-test target, a fixed seconds-scale debug budget and no wall-clock assertion. Follow the brief; do not introduce Pest, nightly or a CI lane.
- AGENTS Dispatch prohibits `matches!` and wildcard arms on owned enums, although AGENTS Tests gives a `matches!` example. Follow Dispatch and the brief: exhaustive matches and structural assertions against typed variants; keep grammar output assertions.
- The generic steering and AGENTS prefer `Fixed:` for a released bug; the brief supplies an exact `Changelog: Changed: shell syntax errors inside $( ), ${ } and here-documents report the script's line`. Use that exact line and do not edit CHANGELOG.md.
- SEQUENCE shared surface 11 says one lock-changing PR in flight; the explicit builder-arm addendum authorizes paired independent PRs for this step. Work only on this arm, leave merge ordering to the coordinator, and merge main plus regenerate both locks with Cargo if its locks advance. Never inspect, base on, merge or comment on the other arm.
- The steering's normal build/push cadence follows this brief's earlier approach gate: push the note now and end the turn; run no build before the note review answers. The ready implementation PR and its fresh coordinator review come afterward.

No other contract conflict was found in PLAN, SEQUENCE rows 32 and shared surfaces 11-13, study sections 4-6, IDEA-0042, the builder-arm rules, or the inspected base. The README currently states neither body-relative lines nor the removed error type names.

## Typed refusal surface

Keep one located parser error vocabulary and lexical reasons as typed leaves:

```rust
enum ParseError {
    Lex { line: usize, refusal: LexError },
    Refused { line: usize, refusal: Refusal },
    TooDeep { line: usize, refusal: TooDeep },
}
```

`LexError` becomes the closed enum of lexical refusal reasons, without a message string or a second located wrapper. `Refusal` covers grammar, unsupported constructs, redirects, patterns and arithmetic syntax. `TooDeep` covers the existing grammar construct, command substitution, parameter and arithmetic-operator limits; the arithmetic-token refusal stays distinct. Retain the existing `Construct` vocabulary for grammar depth.

Each distinct sentence gets a variant or an exhaustive typed subvariant. Variable fields carry a token or optional token for end-of-script, a name, delimiter, character, stream, rejected-command enum, reserved word or construct; they never carry an already formatted sentence. For example, duplication order carries source and copied `Stream`, and a forbidden function carries its name and `RejectedCommand`. Convert the arithmetic rejection table from string-valued reasons to typed reasons too. Move sentence rendering into exhaustive `Display` matches, including existing punctuation, debug quoting and whitespace.

Lexer paths that can invoke the parser return the common `ParseError` directly; lexical-only paths may convert a typed lexical reason at its line. `Parser` stores the pending common error. This removes the `lex_error` Syntax-to-string-to-LexError round trip and avoids mutually boxed parser/lexer errors. Both modules remain private; no exported crate API changes.

Replace unit tests' message helpers and substring assertions, including `nesting_tests.rs`, with assertions against the exact typed variant and its fields. Cover lexical syntax, parameter/index refusals, here-documents, unsupported grammar, redirects, patterns, arithmetic syntax/token limits, and each depth family. Existing end-to-end grammar messages remain the rendering contract.

## Absolute source lines

Use an explicit `start_line` argument to `Parser::new`, nested parse entry points and `Lexer::new`; keep `Depth` concerned with ceilings. Top-level parse starts at 1. A command substitution passes the enclosing lexer's current line immediately after `$(`, and its bounded-prefix reparses reuse that origin. EOF fallback uses the nested origin rather than 1.

Parameter-word and index lexers inherit the current absolute line and initialize word-line tracking consistently. A here-document captures the actual body-start line before scanning each pending body, rather than deriving it from the operator's line; this also handles continued operators and multiple here-documents. Tab stripping changes columns, not lines.

Capture the arithmetic expansion's opening line before `read_balanced` advances, rather than using the start of its containing word or the scanner's final line. Carry absolute line positions through arithmetic scanning where it crosses newlines. Keep byte offsets relative to each existing source slice and preserve consumed-prefix behavior.

Add variant-and-line regressions named for absolute lines in command substitutions, parameter words, here-document bodies and arithmetic expansions. The required three line-3 examples will put a malformed command substitution on script line 3 directly, inside a parameter default word, and inside an interpolated here-document. Add direct lexical/index and multiline arithmetic coverage where the base loses its origin. Demonstrate each new line-number test failing on the base before claiming coverage. Never edit a 2 MiB-stack grammar test. Record every existing grammar assertion whose nested-body line number changes; any required sentence, status or ceiling change stops the work.

## Ordinary-test fuzzing and dependency

Add `arbitrary` 1.4.2 as the shell's sole new dev-dependency, with its shared version declared in workspace dependencies. Dependency rationale for the PR: arbitrary is already locked, provides bounded typed input decoding, and exercises the private parser without adding a production dependency or a new fuzz runtime.

Put one parser no-panic test beside the existing private parser tests. Reuse the existing `two_mib`/`test_threads` ShellThreads permit shape, extending visibility only within test modules if needed. Run the whole fixed corpus in one joined 2 MiB worker; parse and drop both successful ASTs and typed refusals on that worker. Do not catch and discard panics; a failed join fails the test and an abort fails its test process.

Use 1,024 deterministic cases. Decode a reproducible byte corpus with `arbitrary::Unstructured` into bounded fragment selections and depth counts. Generate both incomplete fragments and balanced compositions, biased toward `$(`, `${`, `$((`, quotes, here-documents, case, conditionals, redirects and alternating nesting. Include deep counts through 2,000, while bounding individual scripts to a modest fixed size. Use standard hashing of the case index to populate deterministic decoding bytes, not another random-number dependency. Include Unicode and truncation to exercise character boundaries. Reuse existing deep-script builders where practical. Check that the result is success or the typed error vocabulary; never assert elapsed time.

Do not add a fuzz regression file initially. Any discovered counterexample becomes a small named source regression with its actual failure, and any process-killing defect that cannot be fixed within these contracts is a stop-and-report.

Regenerate stock Cargo.lock with Cargo. In a dedicated isolated worktree, copy the optimized lock into Cargo.lock, resolve using the existing Wasmtime overlay, copy Cargo's result back to `.cargo/wasmtime-optimization.lock`, and test dekopon-shell with the overlay and `--locked`. Never hand-edit locks or change dependency policy.

## Size estimate and validation

The production counter excludes test-only files and cfg(test) items; code-only additionally excludes blank and comment-only lines. It reproduces #797's 12,788 to 12,731 code-only and 13,965 to 13,855 physical totals, and #816's 12,828 to 12,581 code-only and 13,965 to 13,725 physical totals.

| Scope at selected base | Code-only | Physical |
| --- | ---: | ---: |
| Entire shell src, including landed awk parser | 14,787 | 16,012 |
| parser.rs, parser/depth.rs, parser/nested.rs, lexer.rs | 2,728 | 2,951 |

Estimated production delta: 0 to +150 code-only lines, with about +50 to +200 physical lines. This is an estimate, not a measured result. Measure the final entire-shell and affected-source totals by the same counter; include any new production file. Growth above +150 code-only is a recorded choice with exact before/after counts.

At the note stage, only `git diff --check`, the docs duplicate check, source inspection and count reproduction have run. No runtime claim is made for the inherited depth tests yet. After note acceptance, run the full brief gates: fmt, warning-denied workspace Clippy, warning-denied rustdoc, machete, both actual CI daemon dependency steps and their Python tests, provider fixture fetch, workspace lib/bin/test and doctest gates, deny, optimized-lock shell tests, and docs duplicate check. Also follow repository-required release-profile/package and audit-documentation checks applicable to this dependency change. Record every unrun gate and exact tested head/toolchain. The coordinator owns the first twenty post-sync Pi script reads and the fresh Astra PR review.

## Choices I made

- Selected fetched main faacd1b8 because no stamped SHA was included; reported that assumption for review.
- Kept existing shared depth and iterative bounded unary arithmetic rather than repeating landed work from older study text.
- Used lexical leaf variants inside one located ParseError, with separate grammar and depth families and exhaustive sentence rendering.
- Passed explicit start lines and captured actual here-document body origins rather than adding source position to Depth.
- Chose the already-locked arbitrary crate, deterministic bounded cases, and the existing joined ShellThreads test helper.
- Planned 1,024 cases with depths through 2,000 and no initial regression file; discovered failures become named source regressions.
- Applied exact brief wording and its stable-test/no-timing contract over the older steering/study wording where they differ.
- Reproduced the published production counting method; estimated up to +150 code-only and will record any measured overage.
