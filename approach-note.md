# Approach: typed parse refusals and absolute nested lines

Steering read first: xrl/agents `steering/GPT.md` at
`7a5ceaf641b3516be8cfe7b2ff0769ad6690f598`; its first line is
`# Steering for GPT executors`.

This is the approach-note milestone for `10-shell/parser/errors-depth`.
Implementation waits for the coordinator's Astra note review. The requested branch
is `codex/errors-depth-astra-1`; no implementation, manifest or lock edits have begun.

## Base and inputs

The coordinator-stamped base is `faacd1b887416a998c5f48199c505e7bff817d71`
(main after #820). The branch `codex/errors-depth-astra-1` starts at that exact SHA.
Parser, lexer, AST, nesting tests, grammar tests and both lockfiles are unchanged
from the brief's reference `29dd5b6ece0b4fab146214faf55b23eee990a910` (#816).
The cached checkout at `3a73431f` was replaced with the stamped base before any
repository edit. This commit adds only approach-note.md; no PR opens at this stage.

Read the repository AGENTS.md, constitution, CONTRIBUTING.md, development change
maps and validation instructions, PR template, the parser/lexer/depth/nested source,
ArithTooDeep, interpreter parse-error rendering, nesting tests and grammar depth
tests. Also read these campaign-trail sources through GitHub:

- PLAN.md: `store/designs/diaphanous-plan-draft/plan/PLAN.md`, second half of 10-shell and redirect-status F1; blob `cf0072f78d4e9e3457290ac816301e9dda2189cb`.
- SEQUENCE.md: the sibling file, row 32 and shared surfaces 11–13; blob `0b053c312f917fc32a16e7ebbfe61685e5b03540`.
- Parser study: `store/designs/rejigger-design/evidence/studies/opus-pest-shell-parser-pros-cons.md`, sections 4–6; blob `ef6cccf7720af4fff951535437c7212ce60a8d27`.
- IDEA-0042: `store/ideas/IDEA-0042-shell-parser-cleanup.md`; blob `f8bf8581de14ab9970edde33dba28dc2a9acc1d4`.
- Helper rules: `tools/cloud-env/HELPER-RULES.md`; blob `dad3d5bd82ce09df668fbe25a6bac53bbade0496`.
- Builder arms: `store/epochs/diaphanous-dilettantes/15-machinery/review-rubric/study/evidence/builder-arms.md`; blob `0e087666a63983f573f99188cc634a0e8add3fe8`.

## Conflicts and the readings I will follow

1. **Base stamp resolved.** The original dispatch omitted its exact base. The coordinator subsequently stamped `faacd1b887416a998c5f48199c505e7bff817d71` and instructed this note-only branch and push. Use that exact base, including the landed awk parser; leave its implementation untouched.
2. **Historical cleanup versus this step.** IDEA-0042 and study section 5 include Raw-token deletion, redirect deduplication and shared depth. Those changes already exist. PLAN F1 and the current brief place the shared-depth fix in redirect-status. Preserve that implementation; do not repeat the earlier cleanup or alter a ceiling.
3. **Historical unary arithmetic concern.** Study section 4 calls unary recursion unbounded. At the inspected base, `parse_unary` collects prefixes iteratively and applies `ArithExpr::unary`, whose `Height::next` returns `ArithTooDeep` at 64. Preserve both protections and the existing arithmetic token bound.
4. **Study cleanup versus exact sentences.** Preserve the long run of spaces in the conditional-operand refusal and every other existing byte of refusal text. No whitespace correction, grammar expansion or extra remediation wording rides this change.
5. **Study fuzzing versus ordinary CI.** The study's hour-long fuzzing and 10 ms criterion belong to a possible pest spike. Use one fixed-budget stable unit-test entry in the existing library test target, admitted through ShellThreads. No nightly, cargo-fuzz lane, workflow change or wall-clock assertion.
6. **IDEA's typed grammar assertions versus the brief.** Convert parser, lexer and nesting unit tests to typed assertions. Keep `tests/grammar.rs` end-to-end assertions; any edits there may only correct nested line numbers and must be individually listed. All its existing 2 MiB tests remain byte-for-byte unchanged. Typed assertions in `src/nesting_tests.rs` do not change the protected integration tests.
7. **Test target versus private API.** Interpret “one test target” as one fuzz test entry in the existing library target, with its code in a private test-only parser module. A new integration binary must not force an exported parser API or compile a second copy of production modules.
8. **Dependency rule versus test-only dependency.** CONTRIBUTING asks new dependencies to have production consumers, but this brief explicitly requires one fuzz dev-dependency and forbids production additions. Add exactly one direct shell dev-dependency, following the workspace's centralized version convention; no production edge.
9. **Size expectation.** IDEA's roughly −10% describes the whole cleanup, including work that already landed. This step reports its own code-only and physical delta; +150 code-only is a disclosure threshold, not permission to expand unrelated cleanup to offset it.
10. **Changelog category.** Steering generally uses Fixed for shipped bugs, but the brief supplies an exact Changed trailer. Use that trailer verbatim and do not edit CHANGELOG.md.
11. **Lock ordering versus paired arms.** SEQUENCE surface 11 permits one lock-changing PR in flight; the explicit arm dispatch requires two independent branches/PRs for this step. Follow the arm dispatch for this branch only; the coordinator owns admission and merge order. Do not inspect or use the other arm's work. If main's locks move, stop and report the ordering conflict, merge main as directed and regenerate both with Cargo.
12. **Review and delivery.** The newer explicit coordinator-launched Astra note and PR reviews govern over the plan's older default-reviewer description. Push only the approach note at this milestone, then end the turn; open the single implementation PR after implementation and gates. The note is deleted in the final implementation commit.
13. **Reporting.** The brief's short parent report and staged note delivery govern this milestone. The later PR uses repository template headings, a validation table and a final Choices I made section. The required parent `send_message` is not addressable with this environment's agent-tree messaging tool; report that transport gap rather than pretending it was delivered.

No sentence, exit-code, ceiling or protected 2 MiB-test conflict has been found.
The base-dependent claims above are source inspection, not executed-test claims.

## Typed error model

Use one private error model for both lexer and parser. Retain the lexical spelling
as a re-export/alias of the same error type, so there is no conversion, recursive
error boxing or Syntax-to-LexError string round trip:

```rust
enum ParseError {
    Refused { line: usize, refusal: Refusal },
    TooDeep { line: usize, construct: DepthConstruct },
}
// lexer uses the same type under the LexError name.
```

Both variants and their closed payload enums have exhaustive Display matches.
There is no message String, generic Custom(String), wildcard arm, matches! check,
or string comparison on these error enums. Strings are only source data such as
a function name, delimiter, positional digits or numeric literal. A found token
is retained as a typed token, boxed if needed to keep the error value compact,
not rendered before constructing the error. End-of-script is its own found-value
variant. Existing RejectedCommand values carry forbidden-function reasons.

Group refusal payloads around the existing sentences:

| Family | Typed data and coverage |
| --- | --- |
| Unexpected / expected | Found token or EOF; closed expected keyword, syntactic context and punctuation; retain differences such as backticked versus bare unexpected tokens. |
| Unsupported constructs | Closed variants for backticks, select/function syntax, C-style for, array literals, subshell/arithmetic commands, process/input redirection, here-strings, fallthrough, regex and unsupported arithmetic operators. |
| Names and functions | Name text plus name kind; RejectedCommand for forbidden definitions; typed function-body delimiter expectation; function name for background-definition refusal. |
| Redirection | Stream values for order and self-duplication; explicit descriptor/duplication-target refusals; expected buffer and multiple-here-document refusals. |
| Unterminated / empty | Quote, command/arithmetic expansion, parameter/index, delimiter and here-document variants; keep the delimiter string where today's message prints it. |
| Parameters and literal patterns | Name, operator character, colon state, positional digit text; closed pattern context and offending metacharacter. |
| Arithmetic validity and limits | Literal text, offending character/symbol, expected variable/operand, trailing tokens, token ceiling, AST-operator depth; no preformatted reason stored in ARITH_SYMBOLS. |
| Parse depth | Existing block Construct plus command substitution, parameter nesting and arithmetic grouping/operator-depth cases; formatting preserves each current sentence and uses the existing ceilings. |

The concrete enum grouping may be flattened where the nearest existing call sites
make that shorter; every distinct refusal stays expressible without message text
as state. Move refusal literals out of constructors and rejection constants into
Display. The runtime-only `expanded_pattern` helper stays outside this parse-error
refactor. Keep lex-error precedence, bounded-prefix here-document reparsing and
interpreter rendering/exit 2 unchanged.

## Absolute start-line path

Pass `start_line: usize` explicitly to Parser::new and Lexer::new. Depth remains
responsible only for the existing nesting counters. Top-level parsing starts at 1;
Lexer initializes both line and word_line from the supplied start. An empty nested
parser's line fallback uses that start instead of hard-coded 1.

- **Command substitution:** `read_dollar_part` passes its current absolute line to `parse_substitution`; both the initial nested parser and its bounded-prefix reparse use that same starting line. Byte offsets remain relative to the borrowed source slice.
- **Parameter words and indices:** modifier words already copy the parent's current line and cursor; preserve that behavior through the explicit constructor. A separately lexed index currently starts at 1; initialize it at its source line. Any command substitution inside a modifier or index inherits that absolute line.
- **Here-documents:** capture the actual body start from the outer lexer's line immediately before reading each body. Initialize the interpolation lexer at that start. Do not derive every queued body's start from operator-line + 1: earlier queued bodies and continuations can move the actual start. Unterminated-delimiter errors retain the operator's absolute line.
- **Arithmetic:** capture the expansion's opening line before read_balanced consumes it, rather than using the enclosing word's first line or the scanner's ending line. Track newline positions in arithmetic tokenization and retain a line with tokens and EOF. Token-specific refusals use the offending token's absolute line; unterminated constructs retain their opening line. No arithmetic grammar or token/depth budget changes.

Required regressions include these line-3 refusals, with typed payload assertions:

- `:\n:\necho $(echo })`
- `:\n:\necho ${name:-$(echo })}` (direct modifier scanning already keeps its line; this exercises its nested parse)
- `:\ncat <<EOF\n$(echo })\nEOF\n`
- `:\necho $((\n1 ** 2\n))`

The arithmetic case puts `**` on line 3 and the closing `))` on line 4. Source
inspection at the stamped base shows `read_balanced` advances `self.line` to 4
before the lexer passes it to the arithmetic parser, so expecting line 3 exposes
the defect. After note acceptance, verify that failure through the base's existing
error fields, then assert the typed refusal and line 3 after implementation.

Also cover a separately lexed parameter index and multiline nesting composition,
including the here-document bounded-prefix path. Run the line expectations against
the stamped base before the fix using its existing typed wrapper fields to prove
they fail; do not claim failure merely because new enum names do not compile there.
Each refusal family gets typed unit assertions. Existing string-based unit helpers
and nesting refusal comparisons are replaced; existing end-to-end tests keep the
message contract. The current README neither promises body-relative lines nor names
Syntax/LexError; recheck at the stamped base and avoid an unnecessary rewrite.

## Existing bounds to preserve

At the inspected base, Depth::substitution computes the shared lexer + block sum,
then carries it as the nested lexer's depth. Depth::parameter increments that
inherited lexer depth and enforces the existing 32 limit. Parser blocks retain the
64 ceiling and group costs. Arithmetic tokenization retains 4,096 tokens, unary
prefixes are collected without recursion, and ArithExpr enforces a height of 64.

After note acceptance, first run the existing shell baseline tests, including
`alternating_expansions_share_parse_depth_on_a_two_mib_stack`, arithmetic depth
coverage and the source nesting suite. Re-run them unchanged after the refactor.
ShellThreads::STACK_BYTES is exactly 2 * 1024 * 1024 and ThreadPermit::spawn passes
it to the thread builder. No larger test stack is introduced.

## Fuzz crate and generator

Choose proptest, as the sole new direct shell dev-dependency: its mature weighted
generators, deterministic TestRunner and shrinking provide useful parser inputs
and reproducible failures without a custom random generator. Select a published
version compatible with the pinned stable compiler when implementation begins;
use only features needed by the ordinary unit-test runner.

One test, named for parse success-or-typed-refusal without panic on a 2 MiB stack,
uses the existing nesting-test ShellThreads helper shape and a named joined worker.
That worker runs the generator, parse and AST/result destruction, so successful
parses do not escape the stack constraint during drop. A panic fails the join;
an abort fails the test process. No catch-and-ignore panic handling or elapsed-time
assertion is added.

Start with a fixed 2,048-case budget and deterministic seed. Weighted strategies
combine short arbitrary Unicode text, syntax-token fragments and iteratively
wrapped scripts. Bias towards `$(`, `${`, `$((`, single/double quotes and escapes,
`<<`/`<<-` bodies, `case`, `[[`, redirect combinations, unary/binary arithmetic,
newlines, and mixtures of these nested through and beyond existing ceilings.
Include valid closures and truncated/mismatched forms, with bounded script size
(up to 16 KiB), bounded generation depth and bounded shrinking. Measure debug
runtime during development and record any case-budget adjustment; keep it to
seconds, without making elapsed time a test assertion.

Disable filesystem failure persistence for this deterministic ordinary test. If
fuzzing discovers a bug, preserve the minimized script as a named unit regression
and report it; do not commit a redundant generated regression file. Any fix that
would require changing a sentence, exit status, ceiling or protected grammar depth
test is a stop-and-report condition.

## Line counts and estimate

Count all production Rust under crates/dekopon-shell/src, excluding test-only
files and cfg(test) items. Physical counts include blank/comment lines; code-only
counts omit blank and comment-only lines. The read-only counter reproduced #797's
12,788 → 12,731 code-only and 13,965 → 13,855 physical totals, and #816's
12,828 → 12,581 code-only and 13,965 → 13,725 physical totals.

| Scope | Code-only before | Physical before |
| --- | ---: | ---: |
| parser.rs + lexer.rs + parser/depth.rs + parser/nested.rs, stamped base | 2,728 | 2,951 |
| Whole shell at 29dd5b6e | 12,581 | 13,725 |
| Whole shell at stamped base faacd1b8 | 14,787 | 16,012 |

Approach estimate: +80 to +200 code-only production lines and +80 to +250 physical
lines, concentrated in closed error payloads, Display and arithmetic locations.
The affected production scope would be about 2,808–2,928 code-only and
3,031–3,201 physical lines; the whole shell would be about 14,867–14,987
code-only and 16,092–16,262 physical lines. Tests and the development-only dependency
are outside that count. Measure the actual delta at the stamped base and final head; growth
above +150 code-only receives a numbered, reasoned Choices I made entry, not an
unrelated deletion campaign. This note itself changes no production count.

## Implementation validation and delivery

After the accepted note, regenerate the stock lock with Cargo, then regenerate the
optimized lock in an isolated checkout with the documented overlay. Never manually
edit either lock. Check main's lock changes before final validation/push. Keep the
branch rooted at the stamped base, adding merge commits if needed; never amend a
pushed commit. Delete this note in the final implementation commit.

Run and report each requested gate: diff whitespace; formatting; all-workspace,
all-target, all-feature locked Clippy with warnings denied; all-feature locked
rustdoc with warnings denied; cargo machete; both dependency boundaries and their
Python tests; external-provider fixture fetch; locked workspace lib/bin/integration
tests with the documented fixture paths; locked workspace doctests; cargo deny;
optimized-lock shell tests; and the docs duplicate check. Also follow the repository's
applicable dependency/package and release-profile validation instructions, naming
any extra gate run and every omission. No Rust build starts during this note stage.

The implementation commit carries exactly:

`Changelog: Changed: shell syntax errors inside $( ), ${ } and here-documents report the script's line`

The eventual PR title is `refactor(shell): typed parse refusals with absolute nested lines`.
It lists every edited grammar assertion, exact head/toolchain and gate results,
production counts, dependency rationale, remaining gaps and all meaningful choices.
It becomes ready only with passing gates and green CI. The coordinator launches the
fresh Astra PR review and owns merge order and the passive first-twenty-Pi-scripts
check against 0.65.0; that live check remains unresolved here.

## Note-stage checks and outstanding gates

Note-stage validation is diff whitespace and documentation duplicates, including
this note. The local draft passed both checks across 50 Markdown files; repeat them
on the staged root note before committing. Production counts were read from Git
objects and are unchanged by this documentation-only commit.

Not run: cargo fmt, workspace Clippy, rustdoc, machete, both dependency-boundary
steps, test_daemon_dependency_gates.py, external-provider fixture fetch, workspace
lib/bin/integration tests and doctests, cargo deny, optimized-lock shell tests,
release-profile checks, packaging and remote CI. The dispatch requires the pushed
approach-note review before implementation/builds. This milestone commits and
pushes only the note; no PR is opened. The coordinator-owned Astra note review,
later PR review and post-deploy passive check remain outstanding.

## Choices I made

- Branch from the subsequently supplied coordinator stamp faacd1b887416a998c5f48199c505e7bff817d71; commit and push only this note before review, with no PR.
- Use one private ParseError model with a LexError alias/re-export, eliminating the lexical string conversion rather than adding recursive wrappers.
- Group refusal families with closed payload enums and typed source fragments; preserve every existing sentence, including unusual spacing.
- Pass starting lines explicitly through parser and lexer constructors, keeping Depth exclusively about nesting.
- Carry arithmetic token/EOF locations and capture each actual here-document body start to cover all nested-line paths.
- Use one private library fuzz-test entry and the existing ShellThreads admission/join pattern without exporting parse.
- Choose proptest for weighted generation and shrinking; use a fixed seed, fixed case budget and bounded input/shrink sizes.
- Disable generated regression-file persistence; preserve any discovered minimized counterexample as a named unit test.
- Convert unit error assertions to typed payloads while preserving every existing grammar 2 MiB test and end-to-end refusal wording.
- Follow the brief's exact Changed changelog trailer and staged note-then-implementation delivery.
- Report actual code-only and physical growth against the stamped base, explicitly recording any +150 overage.
- Leave paired-arm lock admission, merge order, fresh review and passive deployment verification with the coordinator.
