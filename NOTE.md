# Approach: full Bash IFS for expansion splitting and read

Status: proposal for coordinator review; implementation has not started. The only change at this checkpoint is this note. The branch is `codex/ifs-bash-1`, rooted at exactly `999c6c05a785c7beca52903883aece2b87f8f0a6` after `git fetch origin --tags`. Stop after pushing this note. Delete it in a later commit before the final PR, preserving the reviewable checkpoint in history; never amend a pushed commit.

## Reading and authority

Read first: [GPT steering at cc070358](https://github.com/xrl/agents/blob/cc070358504191cc739c6aaecf124e91da8682aa/steering/GPT.md), whose first line is `# Steering for GPT executors`.

Read at the required base: `AGENTS.md` in full; `docs/design.md` constitution, non-goals, invariants and shell/component ownership; `docs/development.md` repository/change maps, runtime facts, validation, documentation gates and PR checklist; `docs/README.md` area/documentation contracts; `CONTRIBUTING.md`; the PR template; `crates/dekopon-shell/README.md`; relevant shared-runtime sections of `crates/dekopon-agent/README.md`; both CI workflows identifying the Linux runners. Read the named expansion/read functions, their variable/command/substitution owners, the complete replay suite, adjacent grammar helpers/tests and the shared pipe reader and its consumers.

Read [campaign-trail helper rules](https://github.com/dekopon-agents/campaign-trail/blob/main/tools/cloud-env/HELPER-RULES.md), the cloud runtime skill and its networking reference. Runtime status is current/enforced; VPN is unconfigured. Network requests use a generic `codex-agent` User-Agent. No credentials, secret files, authentication inspection or personal-identity request was used. Any owned process is stopped only by its saved PID; no credential file was created.

The user brief authorizes the IFS behavior change and this pushed note, overriding the repository's old current-behavior promises. It does not authorize parser-refusal changes, config, WIT, wire, audit vocabulary, new public API, a compatibility switch, merge, tag, release or deploy. Push only `codex/*`. A need to change a frozen boundary, or absence of Bash 5.2 on a required runner, is a stop, not a reason to shrink coverage.

## Conflicts found before editing and the reading followed

These were reported to the coordinator before creating this file.

| Evidence at the base | Conflict and resolution |
| --- | --- |
| `AGENTS.md` Change and verify; development PR checklist | They normally start from current main. The brief's exact base wins; do not move to a later release stamp. |
| GPT steering's immediate build/push/PR and full-diff report | The explicit note checkpoint wins now: push the note and stop; no implementation or PR yet. |
| Shell README 76-82, 107-110 and 177-178 | They promise default-only splitting, no configurable IFS, inert `-r`, and unsplit array elements. Replace all three locations with the newly requested behavior, plus any adjoining array/join wording made stale. |
| `split_text` 3663; grammar 3972 `assigning_ifs_does_not_configure_field_splitting` | It always splits space/tab/LF, ignoring the variable. Replace this old expectation with unset, empty and custom-IFS behavior. |
| `split_expansion` 3644; `append_unsplit_value` 4469; grammar 3962 | Arrays bypass splitting entirely. Unquoted element expansion must split each element, preserve element boundaries and omit unquoted implicit empty elements as Bash does; quoted `[@]` still preserves each element. |
| `AllPositionalJoined` 3779; `Index::AllJoined` 3915 | Both use a literal space. Quoted joins use the first IFS character, space for unset IFS, and no separator for empty IFS. Unquoted `$*` is not implemented by blindly joining first: empty IFS still leaves separate nonempty parameters. |
| `FieldExpansion` 529 and `split_text` 3663 | A single `separated` Boolean loses the distinction between pending whitespace and nonwhite delimiters across adjacent expansion/quote/literal parts. Replace this private state with the minimum explicit field-building state required by the oracle. |
| `run_read` 1122; grammar 1963 | No-name read is refused and `-r` is discarded. No names instead binds REPLY; default read processes escapes/continuations and `-r` preserves backslashes. Other unsupported flags and invalid-name refusals stay. This is the expressly requested builtin change, not parser widening. |
| `split_read_fields` 4098 | It uses Unicode whitespace, trims only the front and keeps a raw final suffix. Use only space/tab/LF that actually occur in IFS as IFS whitespace; preserve nonwhite empty fields and Bash's last-name remainder rules. |
| `dekopon-process/src/pipe.rs:210` and `run_read` 1150 | `read_line` discards LF and makes partial EOF indistinguishable from a terminated line; read reports success for both and leaves names unchanged at empty EOF. Preserve the LF evidence and assign values before returning Bash's EOF failure status. |
| `local` 3124; `unset` 3163; `lookup` 742 | A declaration without a value becomes an empty string, while unset removes globals and every local frame. That conflates unset/empty IFS and destroys shadowed outer bindings. Extend the existing variable/scope representation so an unset local shadows its outer value and unset acts on the effective binding. No separate IFS scope store. |
| `run_substitution` 3998 | It runs in the parent's evaluator and leaks variable mutations. Use existing scope snapshot/restore ownership for the supported command-substitution environment, keeping shared budgets and existing capture/diagnostic behavior. |
| README's rejected subshells; grammar 1711 | The requested scope coverage cannot mean enabling bare `( ... )` because parser refusal changes are forbidden. Cover supported `$(...)` and non-final pipeline snapshots; list bare subshell syntax as an exclusion. |
| Grammar 4051 `quoted_modifier_words_keep_their_existing_substitution_text` | It expects `prea bpost` where Bash emits `prea\nbpost`. This supported quoted-default divergence is not excluded: fix it along with the expansion path and update the expectation. Check the neighboring quoted dynamic-index expectation at 4059 against the documented JSON value-model extension rather than silently changing unrelated JSON indexing. |
| Real JSON values and array syntax refusal | Preserve typed whole-assignment/object semantics and no filesystem globbing. Compare JSON element expansion with an equivalent Bash indexed-array setup in the oracle harness; do not feed unsupported Bash array literals to Dekopon or claim `$json_array` is Bash scalar-array syntax. |
| Local version versus the brief's patch version | Actual local Bash is 5.2.37, not 5.2.21. The requirement is Bash 5.2; require major/minor 5.2 and report patch versions. The two published CI inventories list 5.2.21; no actual remote execution has been observed. |
| PR template security heading versus GPT steering | The eventual body must explain concrete argv/dispatch effects and unchanged broker authorization under the template heading; do not use an empty boilerplate security section. |

No additional conflict with the constitution, authority boundary, parser refusals or frozen formats was found. If implementation reveals one, stop and report the concrete source and required change.

## Proposed implementation

Use one private IFS policy and one delimiter scanner shared by expansion and read. Resolve IFS through the existing dynamic variable lookup: absent means space/tab/LF; a bound empty rendering disables splitting. Do not read the host environment, cache IFS independently of scope, or special-case assignment syntax. Build character membership once for a splitting operation, with the first UTF-8 character available for joins; avoid scanning the IFS string for every input character.

The scanner walks input once and distinguishes ordinary text, active IFS whitespace and active nonwhite delimiters. Whitespace runs trim/collapse; a nonwhite delimiter with adjacent IFS whitespace forms one delimiter; leading and adjacent nonwhite delimiters produce empty fields; a final delimiter does not create an extra field. Emit spans/delimiter metadata, including the original remainder boundary needed by read, instead of repeatedly splitting or rejoining a suffix. Expansion state survives neighboring parts of one word, and a real array/positional element boundary remains distinct from a delimiter boundary.

Keep quote provenance from the existing AST through field construction. Literal or quoted text is never rescanned as splittable text; selected modifier words preserve their own quotes. Do not flatten a whole word and then split it. Quoted empty strings survive; unquoted implicit nulls disappear. Handle `$v`, `${v}`, selected modifier words and command substitutions through the same mechanism. Preserve assignments, case subjects/patterns, `[[ ]]` and heredoc bodies as unsplit contexts. Keep command substitution's trailing-LF removal separate from IFS splitting.

Treat unquoted `$@`, `$*`, JSON arrays and `[@]` as element sequences with Bash-tested empty-element and concatenation rules. Preserve quoted `$@`/`[@]` as one field per element. Use the first IFS character for quoted `$*`/`[*]`; do not replace every existing `.join(" ")` indiscriminately because JSON rendering and other unsplit contexts have separate contracts.

For read, decode one bounded logical input line while preserving which characters were escaped. With default read, backslash quotes the next character (including an IFS delimiter) and backslash-LF continues the logical line; with `-r`, it is ordinary data. REPLY receives the unsplit logical line, retaining its edge whitespace. Named assignments use the shared scanner; names one through N-1 take fields, and name N gets Bash's remaining text with its internal separators intact and its edge treatment determined by delimiter metadata. Missing fields assign empty values. A partial line at EOF assigns its data but returns failure; empty EOF clears requested names/REPLY and fails. Retain UTF-8 across chunk and continuation boundaries.

Recommended shared-reader adjustment: retain the terminating LF in the bytes returned by the existing `PipeReader::read_line`, with its name/signature/type unchanged, and have xargs remove that LF at its existing line-consumption point. This changes that method's observable byte semantics and must be reviewed explicitly. All workspace uses of this specific reader are read, xargs and its pipe tests; no provider/wire boundary changes. This is smaller than a second buffering/line-reading abstraction in the shell, and preserves the one owner of unread chunk bytes. Add no public type, method or flag. If review requires the public method's old byte semantics to remain immutable, stop before implementing this part rather than silently add a second API.

Scope work remains in the existing variable and snapshot owners: distinguish unset local bindings from explicit empty values, restore prefix assignments after the command, preserve dynamic function lookup, and isolate mutations inside command substitutions and non-final pipeline stages. The last pipeline stage continues to run in the current shell as already documented; compare Bash with `lastpipe` enabled where that existing semantic is relevant. Test that a prefix `IFS=... read` affects read but does not leak, and that argv expansion happens before a command's prefix assignments become effective.

Every emitted field, including empty delimiter fields, charges the existing step/value budget before allocation. Charge retained IFS membership, join output, escape metadata and continuation accumulation to existing ownership; release charges with their owner. Preserve the existing separator-only-input invariant without charging a step per whitespace byte. No suffix rescans, field-count times input-length work, unbounded line/continuation buffer, new thread, queue or dependency is planned.

## Files expected to change

- `crates/dekopon-shell/src/interp.rs`: private field building/policy/scanner, parameter joins, read, scope fixes and the xargs line adaptation; a private child module is acceptable if it makes these owners clearer.
- `crates/dekopon-process/src/pipe.rs`: retain line terminator evidence in the existing reader and update its direct tests; retain bounds and unread-byte ownership.
- `crates/dekopon-shell/tests/grammar.rs`: reuse `Fixture`, `run`, `run_with`, `output`, `worker`; replace old IFS/read/quoted-default expectations, add named regressions and the generated real-Bash corpus. Add only the minimal fixture argv capture needed for exact comparisons.
- `crates/dekopon-shell/tests/replay.rs`: preserve all sixteen deployed recipes and add focused custom-IFS provider-argv/read recipes using `Replay` and `call`; record any changed existing output explicitly rather than normalize it away.
- `crates/dekopon-shell/README.md`: all three required IFS/read/substitution passages and adjoining stale array/join claims.
- `crates/dekopon-shell/src/interp/local_snapshot_tests.rs` only if the existing snapshot unit harness is the right owner for a newly required scope/budget invariant.
- `NOTE.md`: delete after approval in a later commit so it is absent from the final PR diff.

No manifest, dependency, lockfile, parser, AST grammar, WIT, wire, configuration, audit-event, release version or CHANGELOG edit is planned. A newly necessary file is reported with its reason; the brief has no arbitrary diff-size cap.

## Tests first and real-Bash oracle

First add the small regressions that demonstrate current failures, with actual Bash output. Then add a fixed-seed generator that produces exactly 4096 scenarios (in addition to the named tests), verifies its category coverage/count, and runs from ordinary `cargo test`. Print seed, case index, IFS mode, script/setup, expected and actual argument counts and byte vectors on failure. Launch real `bash` from PATH with `expect`; assert 5.2, successful setup and expected process exit status. Missing Bash or the wrong version fails; no skip, ignored test or fallback golden oracle.

Compare argv structurally, not through echo or whitespace-normalized display. Extend the existing fixture to capture the actual argument vector passed through dispatch. Bash's recorder emits count plus NUL-framed argument bytes; the Rust harness decodes that framing and compares vector length and every byte. NUL is a harness separator only, never generated input. For read, record status and every assigned value as distinct arguments. Bound oracle script/output size and execution time and reap any owned child by PID on failure. A test name must identify the behavior it observes, and each new test must have a specific implementation mutation that would make it fail.

The generated families deliberately cover unset/empty/default and individual space/tab/LF IFS; comma, colon, mixed whitespace/nonwhite, repeated punctuation and multibyte UTF-8 delimiters; empty/all-delimiter/leading/trailing/adjacent-delimiter values; literal prefixes/suffixes and adjacent expansion parts; zero/one/many and empty positional/array elements; `$v`, `${v}`, `${v:-w}`, `$(...)`, `$@`, `$*`, `${arr[@]}`, `${arr[*]}`, and quoted variants. Include named and defaulted modifiers in nested words, unsplit assignment/case/conditional/heredoc controls, read with zero through four names, last-name remainders, `-r`, escaped whitespace/nonwhite delimiters, backslash-LF, partial EOF, prefix scope, unset/local/functions, command-substitution scope, loops and pipeline scope. Fixed boundary cases accompany the seed so rare combinations cannot disappear by chance.

For arrays, setup uses the same element bytes in Dekopon's JSON value and Bash's indexed array; compare element expansion semantics, not the intentionally different value constructors. Quote arbitrary generated source with existing supported single/double quoting and literal UTF-8; Bash-only harness setup must not broaden Dekopon's grammar. Keep every discovered supported divergence in coverage: fix it or state a deliberate, reviewable choice with the actual Bash result; never shrink the generator to pass.

Explicit exclusions, each with its reason:

- Bare `( ... )`, Bash array-literal input, here-strings and process substitution: already rejected parser syntax; use supported command substitution/pipelines, JSON arrays and pipes/heredocs for the corresponding behaviors.
- ANSI-C `$'...'` quoting, indirect expansion, sparse/associative arrays and unsupported parameter operators: not in the accepted language; generate bytes through supported quoting.
- `printf -v`, `mapfile`, unsupported read flags and unrelated OS commands: absent builtins/options, not IFS behavior; use the accepted printf/read subset and functions.
- Input NUL and non-UTF-8 bytes: shell text and provider text are UTF-8; transport refusal tests remain intact.
- Locale-specific behavior outside UTF-8: test UTF-8 characters under an explicit UTF-8 locale; no claim for other locale/encoding combinations.
- Filesystem pathname expansion: the shell has no filesystem globbing, already documented; compare only field splitting.

Do not exclude quoted modifier/substitution newline behavior, empty `$*`/array elements, local/unset restoration, partial EOF, or any other discovered supported divergence merely because it fails today.

Named expectations to replace/extend include `substitution_trims_only_trailing_line_feeds_and_splits_on_default_ifs`, `unquoted_strings_and_substitutions_split_every_default_ifs_character`, `array_elements_keep_whitespace_when_expanded_unquoted`, `assigning_ifs_does_not_configure_field_splitting`, `read_binds_several_names_by_splitting_on_whitespace`, `read_refuses_what_it_does_not_implement`, and `quoted_modifier_words_keep_their_existing_substitution_text`. Keep and extend adjacent quote/positional/split-boundary/budget/snapshot tests instead of duplicating them. The eventual PR lists every changed expectation/replay beside its Bash result and any deliberate value-model choice.

## Observed Bash evidence

The following were actually executed locally under `bash --noprofile --norc` 5.2.37. Values below are arrays of exact field strings; `\n` denotes an embedded LF.

| Probe | Observed result |
| --- | --- |
| `IFS=,; v=',a,,b,'; f $v` | Four arguments: `["", "a", "", "b"]`. |
| `IFS=' ,'; v=' , a , , b, '; f $v` | Four arguments: `["", "a", "", "b"]`. |
| Empty IFS, function parameters `['a b', '', 'c']` | Unquoted `$@` and `$*` each produce `["a b", "c"]`; quoted `$@` produces all three; quoted `$*` produces `["a bc"]`. |
| Input `a,b,` plus LF, `IFS=, read x` | `x` is `a,b,`. |
| Same input, `IFS=, read x y` | `x` is `a`, `y` is `b`: last-name handling is not generic trailing-delimiter trimming. |
| Input ` a,b ` plus LF, no names, comma IFS | REPLY is exactly ` a,b `. |
| Input `a,b` without LF, two names | Status 1; values `a`, `b`. |
| Input `a\,b,c` plus LF, default read, two names | Values `a,b`, `c`. |
| Input `a\` plus LF then `b,c` plus LF | Values `ab`, `c`. |
| Global comma IFS, function `local IFS; unset IFS` | Inside, `a,b c` splits to `["a,b", "c"]`; outer comma IFS survives the function. |
| `IFS=,; v=$(IFS=:; printf 'a:b')` | Parent IFS remains comma; `v` is `a:b`. |
| Quoted `${x:-pre$(printf 'a\nb')post}` | One argument containing `prea\nbpost`, not a space. |

## Bash and CI evidence

Actual local runtime: GNU Bash 5.2.37(1)-release, x86_64. The pinned Rust toolchain is 1.98.1; no Rust build/test has been run for this note.

At the required base, `.github/workflows/ci.yml:179` runs workspace tests on `ubuntu-24.04`; line 225 runs component-host tests on `ubuntu-24.04-arm`. `.github/workflows/wasmtime-optimization.yml` also identifies these runner labels; its PR lane is ARM64 and does not execute the shell integration suite.

Published inventories read during this study:

- [x86 Ubuntu 24.04 inventory at 6d35e161](https://github.com/actions/runner-images/blob/6d35e16176367288d91febefafbd221cb2da6ca1/images/ubuntu/Ubuntu2404-Readme.md): image `20261023.334.3`, Bash 5.2.21(1)-release.
- [ARM Ubuntu 24.04 inventory at d14c9de0](https://github.com/actions/partner-runner-images/blob/d14c9de077568464aa602b2009ead4771dac7ea6/images/Ubuntu2404-Readme.md): image `0.0.1`, Bash 5.2.21(1)-release; the partner image overview links to this inventory.

These establish published image contents, not execution on a selected CI job. Actual Bash on either remote runner is still unobserved, and the ARM inventory may lag the selected image. The oracle's mandatory version check will establish x86 execution when the PR suite runs. Before claiming both runners verified, obtain actual runner evidence through the coordinator's authorized CI path; stop if either required runner lacks 5.2. Do not silently widen the workflow or describe inventory evidence as a passing remote check.

## Validation and handoff gates

For this note only: `git diff --check`, the documented duplicate-entry check and CI's audit-event documentation check, then verify the sole changed path is NOTE.md, commit and push. No Rust implementation gate is claimed at this checkpoint.

After note approval, first run focused grammar/oracle/read/scope failures, then implement and run these gates on the resulting head:

- `git diff --check` and `cargo fmt --all --check`.
- `cargo test -p dekopon-shell --all-features --locked`, including grammar, generated Bash corpus, replay and stream tests; `cargo test -p dekopon-process --all-features --locked` for the reader change.
- `cargo test -p dekopon-agent -p dekopon-gatewayd --all-features --locked`, with the documented real broker/provider fixture paths for relevant gateway tests.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`.
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked` and `cargo deny --all-features check`.
- `python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md` and the exact CI audit-event documentation extraction/check; no audit vocabulary changes are planned.
- The applicable root-workspace CI commands in `docs/development.md`: fetch exact ignored provider fixtures, compile workspace tests, run library/binary/integration tests with their real fixture environment, then doctests once; release-profile daemon/console check, cargo-machete, both dependency-boundary gates and their Python test. Name every additional gate actually run and every gate not run with its reason.
- Required remote checks on the exact final head, distinguishing x86 shell/oracle execution from ARM component/optimized checks. Unrelated CI failure needs evidence and a rerun, no unsolicited source fix.

Check artifact ownership and disk before expensive builds; retain only artifacts needed for a named next check and never remove another owner's/active artifacts. Report root-only permission-test limitations exactly if encountered. No provider source, WIT or generated component change is planned, so component rebuild/byte-compare and provider fixture-workspace source edits are not part of the implementation; report any required validation unavailable locally rather than claim it passed.

After gates pass, perform the required adversarial self-review of IFS seams, budgets, quotes, scope, read remainder/escape behavior, all edited comments and actual corpus coverage. Independent reviews belong to the coordinator; the builder does not approve its own work. After implementation gates and the adversarial self-review pass, open one ready-for-review PR; the coordinator then owns external reviews of its immutable head. The PR is titled `shell: full bash IFS for expansion splitting and read`, with the repository template, tested-head/toolchain/gate table, changed tests/replays and Bash evidence, and `Choices I made` at the end. User-visible implementation commits need physical `Changelog:` lines (use Fixed for shipped incorrect behavior); this temporary approach note is not a user-visible implementation commit.

## Choices I made

- Interpret the exact base and explicit note checkpoint as overriding generic current-main/immediate-PR guidance.
- Interpret Bash 5.2 as a major/minor requirement, recording actual patch versions rather than requiring the brief's illustrative 5.2.21 patch locally.
- Cover the requested subshell environment through supported command substitution and pipeline snapshots; retain the existing bare-subshell parser refusal.
- Use one private shared IFS scanner and existing variable/snapshot ownership, with no independent IFS state or compatibility switch.
- Recommend retaining LF in the existing shared line reader's returned bytes, with no new public item, and adapt/test every known consumer; explicitly expose that public semantic change for review.
- Use 4096 fixed-seed generated scenarios plus named cases, with exact argv capture and a required real Bash process.
- Map JSON arrays to equivalent Bash indexed-array setup only in the oracle harness, retaining the documented typed value-model extension.
- Keep last-stage pipeline scope as documented and use Bash lastpipe for equivalent scope probes; do not silently impose Bash's default pipeline subshell behavior.
- Treat the supported quoted-default LF divergence as part of the fix, not a corpus exclusion.
- Separate published runner-image evidence from actual runner execution; no claim that remote CI has run yet.
