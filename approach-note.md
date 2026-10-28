# Approach: case patterns match Bash globs

Pinned steering first line: `# Steering for GPT executors`.

Base: `c16f783926a46ea89d63ea699f03fe9e32a419fd`; branch: `codex/case-globs-sol-1`.
The base contains errors-depth (#821, `9a608554`) and awk pr2 (#822).
Only this note is changed at this stage. Implementation waits for the coordinator-launched
Astra note review; the implementation's final commit will delete this file.

## Conflicts and readings

- The base's parser, evaluator, AST comment and README specify literal-only case patterns.
  The brief deliberately replaces that behavior with Bash globs; parameter and conditional
  pattern refusals retain their present behavior.
- The plan's historical `unsupported_case_pattern` is now `Refusal::CasePattern` in
  `src/parser/refusal.rs`, consumed by `parse_case_pattern`. Delete that typed variant and
  its formatter arm, rather than searching for a function that errors-depth removed.
- The deletion instruction names `expanded_pattern` and `literal_pattern_metacharacter`.
  Both still serve `${NAME#p}`/`%`/`/` and `[[ == ]]`. Remove their case callers, retain
  their other callers, and replace the latter helper's wildcard expectation with exhaustive
  `WordPart` arms. The metacharacter table remains necessary for those other refusals.
- README's broad inert "globbing" sentence becomes ambiguous once case globs exist.
  Qualify it as filesystem pathname expansion, preserving its meaning, remove case globs
  from the rejected list, replace the old literal-case paragraph, and add the kept-case rule.
- Acceptance says every new table row fails at base, but escaped stars, quoted literal
  fragments and bare `*` already work there. Include an unquoted glob alternative in those
  rows (for example, `'*'.json|*.csv` and `*|?`) so the complete scripts fail at base while
  each required behavior remains observable. Keep the existing escaped-pattern test unedited.
- IDEA-0026's old separate-scope status is superseded by IDEA-0042's owner drawdown and
  PLAN section 10-shell, second half. "Fold all three" establishes ownership; this brief
  builds only case globs. IFS and awk behavior do not change.
- The plan's reviewer default and generic branch rule are superseded by the explicit builder
  arm: this branch and coordinator-launched Astra reviews, with no self-review.
- The repository requires the PR template headings, including Security impact, despite the
  steering file's general no-boilerplate rule. Follow the repository-specific template.
- The brief's staged delivery and 300-word parent report supersede the steering file's
  ordinary full-PR-report format for this note handoff. There is no PR or production diff yet.

PLAN section 10-shell, SEQUENCE row 32 and D10 agree on case-globs following errors-depth.
No ordering stop applies. The referenced files were re-found at the stamped base; the brief's
old line numbers are not used as current locations.

## Representation from parsing through expansion

Store each clause's patterns as `Word`s and remove the internal `CasePattern` enum, including
`Any`, `Literal` and `Expanded`. An unquoted star uses the same matcher as every other pattern;
there is no separate default-branch implementation. `ast` is a private module, so this changes
no exported API.

Keep the lexer's existing structure: unquoted text is `WordPart::Literal`, single-quoted text
and backslash-escaped source characters are `SingleQuoted`, and double-quoted parts retain their
container. Expand into a private sequence of `PatternChar::Unquoted(char)` or
`PatternChar::Literal(char)`. Produce these markers while evaluating the word, before any
join can erase quoting. An expansion outside quotes produces unquoted characters; an expansion
inside quotes produces literal characters. Joining preserved element fields retains their
markers and existing separator behavior, without field splitting.

Extend the existing expansion collector with a case-pattern context, reusing parameter,
arithmetic and command-substitution evaluation. This context records lexical quote protection
rather than IFS-derived protection and emits the marked characters directly. Preserve inner
quote markers through selected default/alternate words (`:-`, `-`, `:+`, `+`), so
`${p:-'*'}.json` preserves its inner quoted star.

Assignment modifiers (`:=` and `=`) follow the existing `Modifier::Assign` evaluation in
`interp.rs:3938`: evaluate the selected word without collecting its inner quote markers, assign
the resulting value, then emit that value with protection determined by the enclosing expansion.
Thus, with `p` unset, `${p:='*'}.json` and `${p='*'}.json` produce an active star, whereas
`"${p:='*'}".json` and `"${p='*'}".json` produce literal characters. Inner quote protection
from the assignment word does not survive assignment into an unquoted expansion.
The ordinary expansion contexts and their output remain unchanged; if an oracle row requires
changing their behavior, stop and report the contract conflict.

No matcher input is reconstructed by scanning a quote-stripped String. A backslash originating
in an unquoted expansion has Bash's pattern-escape meaning; a source-escaped backslash is itself
a literal character. The marked representation distinguishes these cases.

## Matcher and bound

Add a private `interp/case_pattern.rs` module with a hand-written glob tokenizer and iterative
dynamic-programming matcher. Tokens cover literals, one-character wildcards, stars, and bracket
members/ranges with optional `!` or `^` negation. Quoting protects bracket delimiters, negation
and range hyphens as well as stars and question marks. Initial `]` and edge `-` are members;
an unmatched `[` is literal. Matching is over Unicode scalar values, case sensitive, and covers
the whole subject. Slash, leading dot and newline have no special exclusion.

Precompute the next active bracket closer in one reverse pass, so repeated unmatched `[` does
not cause repeated suffix scans. Valid bracket expressions consume their contents once.
Use rolling subject-position rows: for a star, `next[j] = previous[j] || next[j - 1]`;
for another token, advance one subject character from `previous[j - 1]` when it matches.
Bracket member checks cost their token's source length. Across all tokens the work is therefore
O(pattern characters × subject characters), including bracket checks, with O(pattern + subject)
scratch memory and no recursive/backtracking search. Charge scratch allocations to the existing
value-byte budget and retain the existing step charge before each attempted pattern.

Refuse active POSIX bracket classes (`[[:alpha:]]`) by name after expansion, with a private typed
matcher error converted through the existing command-failure path. Also name POSIX collating
symbols and equivalence classes if encountered; do not silently interpret those constructs as
literal members. Quoted versions remain literal. No new dependency is needed. `regex-bites`
remains unchanged; a direct glob matcher makes the quote rules and work bound explicit.

## Deletions and tests

Delete the case parse refusal, its typed variant and display arm, the case run-time
`expanded_pattern` refusal path, the `CasePattern` enum and its literal-rule doc comment, and
the wildcard expectation on `literal_pattern_metacharacter`. Delete the stale runtime-test
comment about irretrievably lost quoting.

Rewrite `case_patterns_that_would_glob_are_rejected_by_name` to assert the retained pattern-word
structure and accepted glob syntax. Rewrite `a_case_pattern_assembled_at_run_time_is_still_checked`
to assert matching of its expanded glob. Preserve
`an_escaped_case_pattern_matches_one_literal_character_like_bash` byte-for-byte.
Use the existing `run`/`output` grammar helpers, with one table-driven behavior test and a
separate pathological-pattern result test; add no fixture or timing assertion.

## Bash rows

Observed oracle: GNU Bash `5.2.37(1)-release (x86_64-pc-linux-gnu)`, run with
`bash --noprofile --norc`, `LC_ALL=C.UTF-8`, and `shopt -u extglob nocasematch`.
All 39 rows below were executed successfully before this revision. `yes`/`no` are the selected
branch's output. The extra alternatives make protected-literal and bare-star rows fail at base.

| Subject / setup | Pattern | Output |
| --- | --- | --- |
| `report.json` | `*.json` | yes |
| `abc` | `a?c` | yes |
| `abbc` | `a?c` | no |
| `b` | `[ab]` | yes |
| `bcd` | `[!a]*` | yes |
| `abc` | `[!a]*` | no |
| `bcd` | `[^a]*` | yes |
| `bx` | `[a-c]x` | yes |
| `dx` | `[a-c]x` | no |
| literal `*.json` | `'*'.json` or `*.csv` | yes |
| `report.json` | `'*'.json` or `*.csv` | no |
| `p='*.json'`, literal `*.json` | `"$p"` or `*.csv` | yes |
| `p='*.json'`, `report.json` | `"$p"` or `*.csv` | no |
| `p='*.json'`, `report.json` | `$p` | yes |
| literal `*` | `\*` or `?` | yes |
| `hello` | `\*` or `?` | no |
| `a/b` | `*` or `?` | yes |
| `.hidden` | `*` or `?` | yes |
| literal `[` | `[` | yes |
| `report.json` | `*.txt` or `*.json` | yes |
| empty | `*` or `?` | yes |
| empty | `?` | no |
| `a`, newline, `c` | `a?c` | yes |
| `é` | `?` | yes |
| literal `]` | `[]a]` | yes |
| `b` | `[a'-'c]` | no |
| unset `p`, literal `*.json` | `${p:-'*'}.json` or `*.csv` | yes |
| unset `p`, `report.json` | `${p:-'*'}.json` or `*.csv` | no |
| `p=set`, `report.json` | `${p:+'*'}.json` or `*.csv` | no |
| unset `p`, `report.json` | `${p:='*'}.json` | yes |
| unset `p`, `report.json` | `${p='*'}.json` | yes |
| unset `p`, `report.json` | `"${p:='*'}".json` or `*.csv` | no |
| unset `p`, `report.json` | `"${p='*'}".json` or `*.csv` | no |
| `report.json` | `$(printf '*.json')` | yes |
| `report.json` | `"$(printf '*.json')"` or `*.csv` | no |
| `p='\*'`, literal `*` | `$p` | yes |
| `p='\*'`, `other` | `$p` | no |
| literal backslash followed by `abc` | source `\\*` or `?` | yes |
| `A` | `a?` then `[a]` | no |

The separate pathological row was also run under this Bash: a subject of 8,192 `a` characters
against 64 repetitions of `*a` followed by `b` selects `no`. The Rust test asserts only that
result. POSIX-construct refusal tests exercise the brief's explicit permitted subset boundary,
rather than claiming their refusal equals Bash's matching behavior.

## Validation and handoff

This stage runs `git diff --check` and the repository documentation duplicate check. No Rust
build or implementation test runs before note review. After the review, run every gate named
in the brief: format, workspace Clippy, rustdoc with warnings denied, machete, fixture fetch,
workspace lib/bin/integration tests and doctests, deny, diff check and documentation duplicates;
record any unavailable gate explicitly and verify remote CI on the submitted head.
Report code-only and physical production Rust lines at base and final head using the same
counting method, excluding tests, with the affected-file scope stated. This stage changes no
production line. The feature commit carries exactly:

`Changelog: Added: case patterns match bash globs (*, ?, bracket expressions)`

D10's first-twenty-script passive Pi check and the Mac chat check remain the coordinator's and
owner's post-deploy work. This branch authorizes no merge, tag, release, deploy or live write.

## Choices I made

- Use a private hand-written glob matcher in `interp/case_pattern.rs`, with rolling dynamic-programming rows and no new dependency.
- Carry per-character quote origin in private `PatternChar::Unquoted` / `PatternChar::Literal` variants emitted by case-pattern expansion.
- Extend the existing expansion collector with a case-only context, including selected modifier words, without changing other expansion contexts.
- Preserve inner quote markers for default/alternate words; evaluate assignment words through the existing assignment path and emit the assigned value under the enclosing quote context.
- Refuse POSIX bracket classes, collating symbols and equivalence classes by name rather than approximating them.
- Remove all three internal `CasePattern` variants and store pattern `Word`s directly; bare `*` earns no special case.
- Add active glob alternatives to table rows whose isolated literal or bare-star behavior already passes at base, to meet the each-row-fails-at-base requirement.
- Preserve the shared refusal helpers for parameter and conditional patterns, replacing the wildcard match with exhaustive arms.
- Qualify the README's inert globbing rule as filesystem pathname expansion so it remains true alongside case globs.
