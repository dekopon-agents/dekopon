# Approach: case patterns match Bash globs

Pinned steering first line: “# Steering for GPT executors”.

Base: `c16f783926a46ea89d63ea699f03fe9e32a419fd`.
Its parent contains errors-depth (#821, `9a608554`); the ordering stop does not apply.
This note is the first deliverable for `10-shell/parser/case-globs`.
Implementation waits for the coordinator-launched note review.

## Conflicts and readings

- PLAN §10-shell names `unsupported_case_pattern`; at this base it is
  `parser/refusal.rs::Refusal::CasePattern` plus its display arm. Delete those.
- The brief removes the run-time `expanded_pattern` refusal **for case**. That helper,
  `pattern_metacharacter`, and `literal_pattern_metacharacter` also enforce the
  unchanged `[[ == ]]` and parameter-pattern refusals. Retain those consumers and
  messages; remove only the case consumer. Replace the touched wildcard match with
  exhaustive WordPart arms, removing its `#[expect]`.
- The README has a full literal-only case paragraph as well as the rejected-list
  entry. Delete that stale paragraph, state the new rule in the kept grammar, and
  qualify the inert-globbing entry as filesystem pathname expansion. Remove the
  case comparison in the parameter-pattern paragraph (base README.md:233) and in
  the grep/sed paragraph (base README.md:285), including the blanket claim that a
  script need not know which construct it is in. Preserve the literal-pattern
  rules for those remaining consumers; all other README rules stay unchanged.
- The AST's three CasePattern variants encode literal matching and refusal, rather
  than the new grammar. Remove that private enum and its stale doc comment;
  CaseClause keeps patterns as `Vec<Word>`. The private ast module exports no
  public crate API. Remove WordPart::Literal's newly stale ordinary-character doc
  comment too, as AGENTS Comments requires.
- “Each [table row] fails at the base” cannot literally hold for the required
  already-working controls: quoted literal parts, escaped characters, and bare
  `*` matching slash, leading dot, or empty text. Keep those controls honest;
  identify the newly enabled rows that fail at the base. Do not manufacture a
  failure by putting an unrelated refused construct into a control.
- The arm-specific exact base and branch supersede the generic branch instruction
  and current-main wording. PLAN and SEQUENCE row 32 agree on errors-depth before
  case-globs; both errors-depth and awk pr2 are already merged at the stamped base.
- The brief's note-first, end-turn delivery and short coordinator report supersede
  the steering's normal immediate PR/full-diff report for this turn. A ready PR
  follows implementation, all gates, and the separate fresh review process.

No ordering or outside-case contract stop was found. If implementation needs to
change expansion semantics outside case, stop and report rather than widen them.

## Pattern representation from parsing through expansion

The parser retains the existing Word/WordPart tree without reducing a case word
to constant versus expanded. The lexer already records script backslash escapes
as SingleQuoted parts (`lexer.rs:365`); quoted sections therefore survive parsing.

A private `interp/case.rs` module will own case expansion and matching, following
the existing interpreter submodule layout. Expanded patterns carry a sequence of
`PatternFragment::Literal(char)`, `PatternFragment::Unquoted(char)`, or
`PatternFragment::QuotedNull`. The first two retain per-character quote origin;
the third preserves quoted-empty modifier fragments that have no character to tag.
Literal script text and unquoted expansion results produce Unquoted characters.
SingleQuoted and DoubleQuoted parts protect their characters as Literal, including
quoted expansion results. Empty fragments follow the context-sensitive rules below.
No already-flattened pattern is rescanned to infer its quoting.

Reuse the existing arithmetic, command-substitution, value-rendering, and parameter
evaluators. Extend the optional FieldExpansion output destination into a closed
internal enum with field and case destinations. The existing
`named_parameter_value` modifier dispatch, `expand_selected_word`, and
`finish_parameter` then retain the choice of modifier word and its quote tree
when output goes to a case pattern. Existing field callers retain their current
path. This avoids copying the modifier evaluator or passing case patterns through
IFS splitting.

This distinction matters: Bash preserves the quoted wildcard in
`${p:-'*.json'}` and `${p:+'*.json'}`, but the assignment-result expansion of
`${p:='*.json'}` activates that wildcard when unquoted. The existing Assign
branch already evaluates the replacement separately before emitting the assigned
value. Keep that behavior, including single evaluation and assignment effects.
Quoted outer expansions protect the whole result.

The case destination distinguishes the outer pattern from an unquoted selected
Default/Alternate replacement word. While traversing such a replacement word,
retain each quoted-empty fragment as QuotedNull, even beside nonempty text in that
word; for example, `${p:-''x}` retains the marker before x. Keep markers through
nested modifier expansion and concatenation. Direct empty quotes in the outer
pattern contribute no marker. Quoted outer expansion and assignment-result
expansion retain their existing quote-removal behavior, so `''*.json`,
`"${p:-}"*.json`, and `${p:=''}*.json` do not acquire this marker. This context and
metadata exist only in the case destination; field expansion and non-case pattern
consumers do not change.

After assembling the complete pattern, normalize a sole QuotedNull to the empty
pattern. Otherwise preserve every embedded marker as a literal U+007F character
before glob tokenization; do not discard it or classify the entire pattern as
unmatchable. Thus `${p:-''}*.json` does not match report.json, but does match a
subject beginning with U+007F followed by report.json. Two markers remain two
characters. An actual U+007F from source or expansion stays a character fragment,
never a QuotedNull marker, so it is not erased when it is the whole pattern.
This follows Bash 5.2.37's
[case expansion and QGLOB_CVTNULL conversion](https://github.com/tianon/mirror-bash/blob/c5c97b371044a44b701b6efa35984a3e1956344e/execute_cmd.c#L3603),
[whole-pattern quoted-null test](https://github.com/tianon/mirror-bash/blob/c5c97b371044a44b701b6efa35984a3e1956344e/pathexp.c#L217),
[QUOTED_NULL definition](https://github.com/tianon/mirror-bash/blob/c5c97b371044a44b701b6efa35984a3e1956344e/subst.h#L358), and
[CTLNUL value](https://github.com/tianon/mirror-bash/blob/c5c97b371044a44b701b6efa35984a3e1956344e/shell.h#L143).

Preserve existing case subject expansion and pattern field joining. Pattern
expansion stays unsplit, retains substitution charges, and respects the existing
nesting guard and value-byte accounting. Backslashes produced by an unquoted
expansion receive Bash's pattern-escape handling; quote characters produced by
expansion are ordinary characters, never reparsed shell syntax.

## Matcher and bound

Use a hand-written glob tokenizer and dynamic-programming matcher, with closed
token variants for literal, any character, any sequence, and bracket expression.
No new production dependency. regex-bites remains available to its existing
consumers; this matcher directly consumes character protection without translating
between glob and regex grammars or introducing regex compilation limits.

Normalize quoted-null fragments, then unquoted backslash escapes, then tokenize.
Brackets support members, ranges, initial `!`/`^` negation, literal leading `]`, and literal edge/quoted
hyphens. Quoted punctuation never starts or terminates pattern syntax. An unmatched
opening bracket is literal; later wildcard tokens remain active. Use a right-to-left
next-closing-bracket table so repeated unmatched openings cannot make preprocessing
quadratic. Recognized bracket bodies are consumed once.

Refuse POSIX character classes (`[[:alpha:]]`) with a typed internal refusal that
names “POSIX character class”; likewise name unsupported collating-symbol and
equivalence-class constructs. Detect actual active bracket constructs, not quoted
literal text. These are explicit subset refusals, never silent literal matching.
The ordinary case-glob refusal disappears.

Match Unicode scalar values under the existing UTF-8 value model, using C.UTF-8
Bash oracle cases. `*` and `?` match slash, leading dot, and newline; matching
covers the whole subject and is case-sensitive. No extglob or nocasematch support.

For expanded pattern fragment count P and subject length S, normalization and
tokenization are O(P). A one-row dynamic program records which subject prefixes
the consumed pattern matches: a star uses
the previous row at the same column or the current row's preceding column; other
tokens consume one character from the previous diagonal. Bracket membership work,
summed over a row traversal of all tokens, is at most P per subject character.
Total matching work is O(P × S), with O(P + S) storage; empty inputs cost O(P + S).
There is no recursive wildcard retry. Keep the existing step charge before each
clause pattern; charge matching rows for budget/deadline checks and account for
pattern, subject-character, and DP storage through the existing budget.

## Bash oracle and tests

Observed sandbox oracle: GNU bash 5.2.37(1)-release, x86_64-pc-linux-gnu.
Every row below was run with `LC_ALL=C.UTF-8`,
`bash --noprofile --norc`, and extglob/nocasematch disabled. “yes/no” is the
selected arm's echo output; each script exited 0 with empty stderr.
Where shown, setup `p='*.json'` occurs before the case statement.

| Subject | Pattern / setup | Bash output |
|---|---|---|
| report.json / report.txt | `*.json` | yes / no |
| abc / ac | `a?c` | yes / no |
| b / c | `[ab]` | yes / no |
| bee / apple | `[!a]*` | yes / no |
| bee | `[^a]*` | yes |
| bx / dx | `[a-c]x` | yes / no |
| report.json / *.json | `'*'.json` | no / yes |
| report.json / *.json | `"$p"` | no / yes |
| report.json | `$p` | yes |
| * / abc | `\*` | yes / no |
| a/b / .hidden / empty | `*` | yes / yes / yes |
| [ | `[` | yes |
| bx | `*.json` or `[a-c]x`, separated by a pipe | yes |
| *file.json | `'*'*.json` | yes |
| * | `p='\*';` pattern `$p` | yes |
| report.json | unset p; `${p:-'*.json'}` / `${p:-*.json}` | no / yes |
| report.json | unset p; `${p:='*.json'}` / `${p:=*.json}` | yes / yes |
| report.json | p=x; `${p:+'*.json'}` | no |
| report.json | unset p; `${p:-''}*.json` | no |
| report.json | p=x; `${p:+''}*.json` | no |
| report.json | `''*.json` / unset p; `${p:=''}*.json` | yes / yes |
| empty | unset p; `${p:-''}` / p=x; `${p:+''}` | yes / yes |
| xreport.json | unset p; `${p:-''x}*.json` | no |
| report.json | unset p; `*.json${p:-''}` / `${p:-""}*.json` | no / no |
| empty / two U+007F characters | unset p q; `${p:-''}${q:-''}` | no / yes |
| U+007F followed by report.json | unset p; `${p:-''}*.json` | yes |
| empty | unset p; `''${p:-''}` | yes |
| report.json | unset p; `"${p:-}"*.json` / `"${p:-""}"*.json` | yes / yes |
| one U+007F character | p contains U+007F; `$p` | yes |
| report.json | `$(printf '*.json')` | yes |
| ] | `[]a]` / `[a"]"]` | yes / yes |
| b | `[a'-'c]` | no |
| é / one newline between a and b | `?` / `a?b` | yes / yes |
| [abc | `[a*` | yes |
| [a] | `'['a]` | yes |
| ^ | `['^'a]` | yes |
| one backslash | p contains one backslash; `$p` | yes |
| a | `[[:alpha:]]` / `[[.a.]]` / `[[=a=]]` | yes; deliberately refused by name in this subset |
| 4,096 a characters | 128 repetitions of `*a` followed by b | no |

Use the existing grammar-test `run`/`output` helpers for one table-driven
behavior test. Newly accepted glob rows and quoted-variable rows expose the base's
parse/run-time refusals; the literal and universal-star rows remain controls.
Rerun every exact checked-in script under Bash before reporting its expectation.
Include the quoted-empty modifier rows and direct-quote/assignment controls in
that table: dropping the marker makes the default/alternate rows fail, while
emitting it for every empty quote makes the controls fail. The whole-pattern and
U+007F rows distinguish normalization from dropping every marker or rejecting
every pattern that contains one.

Rewrite `case_patterns_that_would_glob_are_rejected_by_name` into a parser test
that checks accepted pattern words and preserved quote parts. Rewrite
`a_case_pattern_assembled_at_run_time_is_still_checked` to match a pattern
assembled from multiple expansions, avoiding a duplicate of the simple $p row.
Leave `an_escaped_case_pattern_matches_one_literal_character_like_bash` unedited.
The separate pathological test asserts only the no-match output, with no timing
assertion. Assert named subset refusals through their typed internal variants.
Keep the existing step-budget and non-case refusal tests.

## Deletions, validation, and delivery

Delete the generic case parse-refusal variant/display, the case run-time
expanded-pattern check, the obsolete CasePattern enum and doc comment, the
obsolete parser test assertions, the run-time-refusal test's stale comment,
and the touched literal_pattern_metacharacter wildcard expectation.
Retain shared refusal helpers and their non-case behavior.

The final feature commit carries exactly:
`Changelog: Added: case patterns match bash globs (*, ?, bracket expressions)`.
Do not edit CHANGELOG.md. Delete this note in that final commit.

This note changes zero production lines (code-only and physical); before/after
production totals will be measured on the implementation diff with the counting
scope and method recorded. No Rust build has been run for the note stage. The docs
duplicate check passed (49 files), and CI's audit-event documentation check passed
(45 names). The initial 44 Bash oracle scripts completed successfully; this note
fix adds 20 observed quoted-empty and control probes, including repeated and nested
modifier fragments; all exited 0 with empty stderr.

After the note review, run: git diff --check; cargo fmt --all --check;
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings;
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked;
cargo machete; ci/fetch-external-provider-components.sh examples/providers;
cargo test --workspace --all-features --locked --lib --bins --tests;
cargo test --workspace --all-features --locked --doc;
cargo deny --all-features check; and the documented docs duplicate check.
Also run CI's audit-event documentation gate. Name every unavailable gate.
Only mark the implementation PR ready after the required gates pass and observe
remote CI. The coordinator launches the fresh review and owns D10's passive Pi
check and the Mac chat check; this branch does not merge, release, or deploy.

## Choices I made

- Use a private interp/case.rs module and hand-written dynamic programming over tagged characters.
- Preserve Literal/Unquoted characters plus QuotedNull fragments; normalize the whole pattern before tokenization.
- Reuse parameter modifier dispatch with a case output destination so other expansion behavior stays fixed.
- Refuse POSIX character classes, collating symbols, and equivalence classes by their construct names.
- Remove all three CasePattern variants and use Word directly; bare * needs no separate AST form.
- Keep shared non-case refusal helpers and remove only the case refusal paths.
- Read “each fails at the base” as applying to new-behavior rows; keep already-supported rows as controls.
- Remove stale case comparisons from all three README paragraphs and qualify inert globbing as filesystem pathname expansion.
- Use Bash 5.2 under C.UTF-8 for deterministic Unicode and range examples.
