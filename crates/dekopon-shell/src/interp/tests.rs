//! Evaluator behavior tests, including the kept-versus-dropped grammar contract.

use std::{cell::RefCell, time::Duration};

use serde_json::{Value, json};

use crate::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, ExitCode, Interpreter, Limits,
    ScriptOutcome,
};

/// A fixture exposing a few capabilities with distinguishable outcomes.
#[derive(Default)]
pub(super) struct Fixture {
    pub(super) calls: RefCell<Vec<(String, Value)>>,
}

impl CapabilityInvoker for Fixture {
    fn granted(&self) -> Vec<String> {
        vec![
            "echo.echo".to_owned(),
            "http-probe.fetch".to_owned(),
            "policy.denied".to_owned(),
            "provider.broken".to_owned(),
        ]
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        (capability == "echo.echo").then(|| CapabilityDescription {
            capability: capability.to_owned(),
            description: "Echoes its input".to_owned(),
            input_schema: json!({"type": "object"}),
        })
    }

    fn invoke(&self, capability: &str, input: Value) -> CapabilityCallResult {
        self.calls
            .borrow_mut()
            .push((capability.to_owned(), input.clone()));
        match capability {
            "policy.denied" => CapabilityCallResult::Denied {
                reason: "exact policy refused this proposal".to_owned(),
            },
            "provider.broken" => CapabilityCallResult::Failed {
                error: "provider trapped".to_owned(),
            },
            "http-probe.fetch" => CapabilityCallResult::Succeeded(json!({
                "status": 200,
                "bodyText": "alpha\nbeta\nalpha",
            })),
            _ => CapabilityCallResult::Succeeded(input),
        }
    }
}

fn run(script: &str) -> ScriptOutcome {
    Interpreter::new(Limits::default()).run(script, &Fixture::default())
}

fn run_with(script: &str, limits: Limits) -> ScriptOutcome {
    Interpreter::new(limits).run(script, &Fixture::default())
}

fn output(script: &str) -> String {
    run(script).output
}

fn code(script: &str) -> u8 {
    run(script).exit_code.get()
}

// ---------------------------------------------------------------------------
// Kept grammar
// ---------------------------------------------------------------------------

#[test]
fn assigns_and_expands_variables() {
    assert_eq!(output("name=world\necho \"hello $name\""), "hello world");
    assert_eq!(output("x=1\ny=$x\necho ${y}"), "1");
    assert_eq!(output("echo \"[$missing]\""), "[]");
}

#[test]
fn quoting_matches_bash() {
    assert_eq!(output(r#"x=v; echo '$x $(echo no)'"#), "$x $(echo no)");
    assert_eq!(output(r#"x=v; echo "$x""#), "v");
    assert_eq!(output(r#"echo "a\"b""#), "a\"b");
}

#[test]
fn sequencing_and_short_circuiting_follow_exit_codes() {
    assert_eq!(output("true && echo yes"), "yes");
    assert_eq!(output("false && echo no"), "");
    assert_eq!(output("false || echo fallback"), "fallback");
    assert_eq!(output("true || echo skipped"), "");
    assert_eq!(output("echo a; echo b"), "a\nb");
}

#[test]
fn last_status_is_observable() {
    assert_eq!(output("true; echo $?"), "0");
    assert_eq!(output("false; echo $?"), "1");
    assert_eq!(
        output("nope.missing; echo $?"),
        "dekopon-shell: nope.missing: command not found\n127"
    );
}

#[test]
fn comments_are_ignored() {
    assert_eq!(
        output("# leading\necho hi # trailing\n# trailing only"),
        "hi"
    );
}

#[test]
fn if_elif_else_selects_one_branch() {
    let script = "x=2\nif [ $x -eq 1 ]; then echo one; elif [ $x -eq 2 ]; then echo two; else echo other; fi";
    assert_eq!(output(script), "two");
    assert_eq!(output("if false; then echo a; else echo b; fi"), "b");
}

#[test]
fn for_loops_iterate_over_words_and_arrays() {
    assert_eq!(output("for x in a b c; do echo $x; done"), "a\nb\nc");
    // An unquoted `$( )` producing a real JSON array expands element by element.
    assert_eq!(
        output("for x in $(echo.echo --a 1 --b 2 --c 3 | jq '[.a,.b,.c]'); do echo $x; done"),
        "1\n2\n3"
    );
}

#[test]
fn while_and_until_loops_terminate_on_their_condition() {
    assert_eq!(
        output("i=0\nwhile [ $i -lt 3 ]; do echo $i; i=$(( i + 1 )); done"),
        "0\n1\n2"
    );
    assert_eq!(
        output("i=0\nuntil [ $i -ge 2 ]; do echo $i; i=$(( i + 1 )); done"),
        "0\n1"
    );
}

#[test]
fn break_and_continue_respect_nesting_levels() {
    assert_eq!(
        output("for x in 1 2 3; do if [ $x -eq 2 ]; then continue; fi; echo $x; done"),
        "1\n3"
    );
    assert_eq!(
        output("for x in 1 2 3; do if [ $x -eq 2 ]; then break; fi; echo $x; done"),
        "1"
    );
    // `break 2` leaves both loops; only the outer loop's trailing marker should print.
    assert_eq!(
        output(
            "for a in 1 2; do for b in 1 2; do echo $a$b; if [ $b -eq 1 ]; then break 2; fi; done; echo inner-done; done\necho after"
        ),
        "11\nafter"
    );
    // `continue 2` restarts the outer loop instead of the inner one.
    assert_eq!(
        output(
            "for a in 1 2; do for b in 1 2; do echo $a$b; continue 2; done; echo unreachable; done"
        ),
        "11\n21"
    );
}

#[test]
fn functions_take_positional_parameters_and_return_status() {
    let script = "greet() { echo \"hi $1 and $2\"; }\ngreet ann bob";
    assert_eq!(output(script), "hi ann and bob");
    assert_eq!(output("count() { echo $#; }\ncount a b c"), "3");
    assert_eq!(output("all() { echo $@; }\nall a b"), "a b");
    assert_eq!(output("fail() { return 3; }\nfail; echo $?"), "3");
}

#[test]
fn a_negated_pipeline_inverts_its_status() {
    // Dispatching `!` as a command word reported "command not found" and inverted every branch.
    assert_eq!(output("if ! false; then echo neg; fi"), "neg");
    assert_eq!(output("if ! true; then echo no; else echo yes; fi"), "yes");
    assert_eq!(output("! false && echo reached"), "reached");
    assert_eq!(code("! true"), 1);
    assert_eq!(code("! false"), 0);
    // A `!` that is an argument rather than a pipeline prefix is untouched.
    assert_eq!(output("if [ ! -z x ]; then echo arg; fi"), "arg");
}

#[test]
fn functions_participate_in_pipelines_in_both_directions() {
    // A function used to leak its output past the pipe and hand the next command a null.
    assert_eq!(output("f() { echo hi; }\nf | wc -c"), "2");
    assert_eq!(output("g() { cat; }\necho payload | g"), "payload");
    // A guard that never reads the input must not swallow it before the command that does.
    assert_eq!(
        output("g() { if [ -n \"$1\" ]; then cat; fi; }\necho payload | g yes"),
        "payload"
    );
    assert_eq!(output("f() { echo one; echo two; }\nf | grep one"), "one");
    // In terminal position a function still streams straight to the output.
    assert_eq!(output("f() { echo one; echo two; }\nf"), "one\ntwo");
    // A function that produces nothing contributes no phantom line.
    assert_eq!(output("echo a\nq() { true; }\nq\necho b"), "a\nb");
}

#[test]
fn prefix_assignments_are_transient_and_applied_after_expansion() {
    // `x=new echo "[$x]"` must print the *old* value and must not outlive the command.
    assert_eq!(
        output(r#"x=old; x=new echo "[$x]"; echo "after=[$x]""#),
        "[old]\nafter=[old]"
    );
    // A prefix assignment on a name that did not exist leaves no binding behind.
    assert_eq!(output(r#"DEBUG=1 true; echo "[$DEBUG]""#), "[]");
    // An assignment with no command word is an ordinary, lasting assignment.
    assert_eq!(output(r#"x=kept; echo "[$x]""#), "[kept]");
}

#[test]
fn shift_consumes_positional_parameters() {
    assert_eq!(output(r#"f() { shift; echo "$1"; }; f a b"#), "b");
    assert_eq!(output(r#"f() { shift 2; echo "$1 $#"; }; f a b c"#), "c 1");
    // Shifting past the end fails rather than silently truncating, as in bash.
    assert_eq!(output(r#"f() { shift 5; echo $?; }; f a"#), "1");
    assert_eq!(
        output(r#"f() { while [ $# -gt 0 ]; do echo $1; shift; done; }; f a b c"#),
        "a\nb\nc"
    );
}

#[test]
fn quoted_all_positional_splits_one_word_per_parameter() {
    // `for x in "$@"` is the most-trained argument idiom there is; joining it into one word made
    // the quoted form silently wrong and the unquoted form the only correct one.
    assert_eq!(
        output(r#"f() { for a in "$@"; do echo "[$a]"; done; }; f "one two" three"#),
        "[one two]\n[three]"
    );
    assert_eq!(
        output(r#"f() { count() { echo $#; }; count "$@"; }; f a b c"#),
        "3"
    );
    // Zero parameters forward as zero words, not one empty one.
    assert_eq!(
        output(r#"f() { count() { echo $#; }; count "$@"; }; f"#),
        "0"
    );
    // `$*` is the always-joined counterpart, and used to expand to the literal text `$*`.
    assert_eq!(output(r#"f() { echo "[$*]"; }; f a b"#), "[a b]");
    assert_eq!(
        output(r#"f() { count() { echo $#; }; count "$*"; }; f a b"#),
        "1"
    );
}

#[test]
fn diagnostics_inside_a_substitution_still_reach_the_output() {
    // Only the *value* of `$( )` is captured. Swallowing its errors too left a script with an
    // empty variable, no explanation, and a `$?` it may never look at.
    let outcome = run(r#"v=$(nosuchcmd.here); echo "v=[$v] status=$?""#);
    assert!(
        outcome.output.contains("nosuchcmd.here: command not found"),
        "{}",
        outcome.output
    );
    assert!(
        outcome.output.contains("v=[] status=127"),
        "{}",
        outcome.output
    );

    let outcome = run(r#"v=$(policy.denied); echo "[$v]""#);
    assert!(
        outcome
            .output
            .contains("exact policy refused this proposal"),
        "{}",
        outcome.output
    );
}

#[test]
fn an_interpolated_substitution_still_reports_its_status() {
    assert_eq!(output("x=a$(false); echo $?"), "1");
    assert_eq!(output("x=$(false); echo $?"), "1");
    assert_eq!(output("x=a$(true); echo $?"), "0");
}

#[test]
fn local_shadows_a_global_with_bash_dynamic_scoping() {
    let script = "\
x=global
inner() { echo $x; }
outer() { local x=shadowed; inner; }
outer
echo $x";
    // Dynamic scoping: `inner` sees `outer`'s local, and the global is intact afterwards.
    assert_eq!(output(script), "shadowed\nglobal");
}

#[test]
fn recursion_works_within_the_depth_cap() {
    let script = "\
countdown() {
  if [ $1 -le 0 ]; then return 0; fi
  echo $1
  countdown $(( $1 - 1 ))
}
countdown 3";
    assert_eq!(output(script), "3\n2\n1");
}

#[test]
fn arithmetic_expansion_covers_the_documented_operators() {
    assert_eq!(output("echo $(( 1 + 2 * 3 ))"), "7");
    assert_eq!(output("echo $(( (1 + 2) * 3 ))"), "9");
    assert_eq!(output("echo $(( 7 / 2 ))"), "3");
    assert_eq!(output("echo $(( 7 % 2 ))"), "1");
    assert_eq!(output("echo $(( 7.0 / 2 ))"), "3.5");
    assert_eq!(output("echo $(( 2 < 3 ))"), "1");
    assert_eq!(output("echo $(( 2 >= 3 ))"), "0");
    assert_eq!(output("echo $(( 1 == 1 ))"), "1");
    assert_eq!(output("echo $(( 1 != 1 ))"), "0");
    assert_eq!(output("echo $(( 1 && 0 ))"), "0");
    assert_eq!(output("echo $(( 1 || 0 ))"), "1");
    assert_eq!(output("n=5; echo $(( n * 2 ))"), "10");
    assert_eq!(output("n=5; echo $(( $n - 1 ))"), "4");
}

#[test]
fn division_by_zero_is_recoverable_not_fatal() {
    let outcome = run("echo $(( 1 / 0 ))\necho after");
    assert!(
        outcome.output.contains("division by zero"),
        "{}",
        outcome.output
    );
    assert!(outcome.output.contains("after"), "{}", outcome.output);
}

#[test]
fn command_substitution_preserves_structure_only_as_a_whole_rhs() {
    // Whole-RHS `$( )` keeps the structured value...
    assert_eq!(
        output(r#"r=$(echo.echo --status 200); echo ${r[status]}"#),
        "200"
    );
    // ...while an interpolated `$( )` coerces to display form, exactly like bash.
    assert_eq!(
        output(r#"r="x$(echo.echo --status 200)"; echo $r"#),
        r#"x{"status":200}"#
    );
}

#[test]
fn indexing_is_backed_by_real_json() {
    assert_eq!(output(r#"o=$(echo.echo --a 1 --b 2); echo ${o[b]}"#), "2");
    assert_eq!(
        output(r#"a=$(echo.echo --x 10 | jq '[.x, 20]'); echo ${a[1]}"#),
        "20"
    );
    assert_eq!(
        output(r#"a=$(echo.echo --x 10 | jq '[.x]'); echo "[${a[9]}]""#),
        "[]"
    );
}

#[test]
fn unquoted_arrays_expand_element_by_element() {
    // POSIX IFS splitting is dropped; a JSON array is what produces multiple argv words.
    assert_eq!(
        output(r#"a=$(echo.echo --x x --y y | jq '[.x,.y]'); count() { echo $#; }; count $a"#),
        "2"
    );
    // A scalar containing spaces stays exactly one word.
    assert_eq!(
        output(r#"s="one two"; count() { echo $#; }; count $s"#),
        "1"
    );
}

#[test]
fn pipelines_deliver_structured_values() {
    assert_eq!(output(r#"echo.echo --a 1 | jq .a"#), "1");
    assert_eq!(output("echo 'a\nb\na' | sort | uniq | wc -l"), "2");
    assert_eq!(
        output("http-probe.fetch --uri x | jq -r .bodyText | grep alpha | wc -l"),
        "2"
    );
}

#[test]
fn redirection_writes_and_cat_reads_named_buffers() {
    assert_eq!(output("echo hi > buf\ncat buf"), "hi");
    assert_eq!(output("echo a > buf\necho b >> buf\ncat buf | wc -l"), "2");
    assert_eq!(output("echo hi > buf"), "");
    let outcome = run("cat /etc/passwd");
    assert!(
        outcome.output.contains("no such buffer"),
        "{}",
        outcome.output
    );
}

#[test]
fn exit_sets_the_script_status_and_wraps_like_bash() {
    assert_eq!(code("exit 0"), 0);
    assert_eq!(code("exit 7"), 7);
    assert_eq!(code("exit 300"), 44);
    assert_eq!(output("echo a; exit 1; echo b"), "a");
    assert_eq!(code("echo a; exit 1; echo b"), 1);
}

#[test]
fn xargs_maps_a_command_over_a_list() {
    let fixture = Fixture::default();
    let outcome = Interpreter::new(Limits::default()).run(
        r#"echo.echo --a a --b b | jq '[.a,.b]' | xargs cap echo.echo --name"#,
        &fixture,
    );
    // One call builds the list, then `xargs` drives one call per element.
    assert_eq!(outcome.capability_calls, 3);
    let calls = fixture.calls.borrow();
    assert_eq!(calls[1].1, json!({"name": "a"}));
    assert_eq!(calls[2].1, json!({"name": "b"}));
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
}

// ---------------------------------------------------------------------------
// Capability dispatch and exit codes
// ---------------------------------------------------------------------------

#[test]
fn a_granted_capability_is_callable_as_a_bare_command() {
    let fixture = Fixture::default();
    let outcome =
        Interpreter::new(Limits::default()).run("echo.echo --post-id 7 --include-body", &fixture);
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
    assert_eq!(
        fixture.calls.borrow()[0].1,
        json!({"postId": 7, "includeBody": true})
    );
}

#[test]
fn capability_outcomes_map_onto_their_documented_exit_codes() {
    assert_eq!(code("echo.echo --a 1"), 0);
    assert_eq!(code("provider.broken"), 1);
    assert_eq!(code("policy.denied"), 126);
    assert_eq!(code("not.granted"), 127);
    assert_eq!(code("definitelynotacommand"), 127);
}

#[test]
fn cap_lists_and_describes_capabilities() {
    assert!(output("cap --list").contains("echo.echo"));
    assert!(output("cap --describe echo.echo").contains("Echoes its input"));
}

#[test]
fn a_function_shadows_a_builtin_only_when_declared_first() {
    assert_eq!(output("echo hi"), "hi");
    assert_eq!(output("echo() { true; }\necho hi"), "");
}

// ---------------------------------------------------------------------------
// Dropped grammar: every one of these must fail loudly, not silently
// ---------------------------------------------------------------------------

#[test]
fn globbing_is_dropped_and_stays_literal() {
    // There is no filesystem to glob against, so `*` is an ordinary character.
    assert_eq!(output("echo *"), "*");
    assert_eq!(output("echo a?b"), "a?b");
    assert_eq!(output("echo [abc]"), "[abc]");
    assert_eq!(code("echo *"), 0);
}

#[test]
fn brace_and_tilde_expansion_are_dropped_and_stay_literal() {
    assert_eq!(output("echo {a,b,c}"), "{a,b,c}");
    assert_eq!(output("echo ~"), "~");
    assert_eq!(output("echo ~/x"), "~/x");
}

#[test]
fn backgrounding_is_a_hard_parse_error() {
    let outcome = run("sleep 1 &\necho after");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(
        outcome.output.contains("backgrounding"),
        "{}",
        outcome.output
    );
    // Nothing ran: a parse failure rejects the whole script.
    assert!(!outcome.output.contains("after"), "{}", outcome.output);
}

#[test]
fn eval_is_rejected_as_a_sandbox_escape() {
    let outcome = run("eval 'echo hi'");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(outcome.output.contains("eval"), "{}", outcome.output);
    assert!(
        outcome.output.contains("self-modifying code"),
        "{}",
        outcome.output
    );
    assert!(!outcome.output.contains("hi"), "{}", outcome.output);
}

#[test]
fn ambient_authority_commands_are_rejected_by_name() {
    for (script, expected) in [
        ("exec echo hi", "exec"),
        ("source other.sh", "source"),
        (". other.sh", "source"),
        ("trap x INT", "trap"),
        ("wait", "wait"),
        ("jobs", "jobs"),
        ("kill 1", "kill"),
        ("declare -A m", "declare"),
        ("export X=1", "export"),
    ] {
        let outcome = run(script);
        assert_eq!(outcome.exit_code, ExitCode::SYNTAX, "{script}");
        assert!(
            outcome.output.contains(expected),
            "{script}: {}",
            outcome.output
        );
    }
}

#[test]
fn subshells_here_documents_and_process_substitution_are_rejected() {
    for (script, expected) in [
        ("(echo hi)", "subshells"),
        ("{ echo hi; }", "brace command groups"),
        ("cat <<EOF\nx\nEOF", "here-documents"),
        ("diff <(echo a) b", "process substitution"),
        ("cat < file", "input redirection"),
        ("case $x in esac", "case"),
    ] {
        let outcome = run(script);
        assert_eq!(outcome.exit_code, ExitCode::SYNTAX, "{script}");
        assert!(
            outcome.output.contains(expected),
            "{script}: {}",
            outcome.output
        );
    }
}

#[test]
fn bash_array_emulation_is_rejected_in_favor_of_json() {
    let outcome = run("echo ${arr[@]}");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(outcome.output.contains("JSON array"), "{}", outcome.output);
}

#[test]
fn shell_shapes_this_interpreter_cannot_honor_are_rejected_by_their_own_name() {
    // Each of these used to succeed quietly, or fail while naming the wrong feature. The message
    // has to identify what the script actually wrote, or it sends a reader to the wrong fix.
    for (script, expected) in [
        // Backticks: the second-most-common substitution form in real bash.
        ("echo `echo hi`", "backtick command substitution"),
        ("x=`date`", "backtick command substitution"),
        // Numbered descriptors: this shell has one combined stream.
        ("echo hi 2>/dev/null", "file-descriptor redirection"),
        ("echo hi >&2", "file-descriptor redirection"),
        ("echo hi 2>&1", "file-descriptor redirection"),
        // Shell options: `set -e` changing nothing while looking like it had is the exact
        // failure this design forbids.
        ("set -euo pipefail\necho after", "no shell options"),
        ("[[ -n \"x\" ]] && echo yes", "[[ ... ]]"),
        // Paren-shaped constructs are four different features, not one subshell.
        ("i=0; ((i++))", "arithmetic command"),
        ("arr=(a b c)", "bash array literals"),
        ("for ((i=0; i<3; i++)); do echo $i; done", "C-style"),
        ("(echo hi)", "subshells"),
        // Arithmetic operators name themselves rather than a stray character.
        ("echo $((2 ** 3))", "`**` is not supported"),
        ("echo $((i++))", "`++` is not supported"),
        ("echo $((i += 2))", "compound assignment"),
        ("echo $(( 1 > 0 ? 5 : 6 ))", "ternary"),
        ("echo $(( 1 & 2 ))", "bitwise"),
    ] {
        let outcome = run(script);
        assert_ne!(outcome.exit_code, ExitCode::SUCCESS, "{script}");
        assert!(
            outcome.output.contains(expected),
            "{script}: {}",
            outcome.output
        );
        assert!(!outcome.output.contains("after"), "{script}: ran anyway");
    }
}

#[test]
fn a_non_ascii_character_in_arithmetic_is_named_as_itself() {
    // Casting the raw byte reported 'Ã', the first UTF-8 byte of 'é' — a character the script
    // never wrote, in a project whose diagnostics are the product.
    let outcome = run("echo $(( 1 é 2 ))");
    assert!(outcome.output.contains("'é'"), "{}", outcome.output);
}

#[test]
fn a_text_builtin_that_selected_nothing_emits_nothing() {
    // An empty result used to render as a blank line, which also spent a line of the ceiling.
    assert_eq!(
        output("echo start; echo a | grep zzz; echo end"),
        "start\nend"
    );
    assert_eq!(
        output("echo start; echo a | grep zzz | wc -l; echo end"),
        "start\n0\nend"
    );
}

// ---------------------------------------------------------------------------
// Sandbox limits
// ---------------------------------------------------------------------------

#[test]
fn the_step_budget_stops_an_unbounded_loop() {
    let outcome = run_with(
        "while true; do x=1; done",
        Limits {
            max_steps: 500,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(
        outcome.output.contains("step budget exhausted"),
        "{}",
        outcome.output
    );
    assert!(outcome.steps <= 501);
}

#[test]
fn the_recursion_cap_stops_runaway_shell_functions() {
    let outcome = run_with(
        "recurse() { recurse; }\nrecurse",
        Limits {
            max_recursion_depth: 16,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(
        outcome.output.contains("nested deeper"),
        "{}",
        outcome.output
    );
}

#[test]
fn the_capability_call_cap_is_independent_of_the_step_budget() {
    let outcome = run_with(
        "for i in 1 2 3 4 5; do echo.echo --i $i; done",
        Limits {
            max_capability_calls: 2,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert_eq!(outcome.capability_calls, 2);
    assert!(
        outcome.output.contains("more than 2 capability calls"),
        "{}",
        outcome.output
    );
}

#[test]
fn deeply_nested_input_is_a_syntax_error_rather_than_a_dead_process() {
    // Every recursive production runs on the native stack before any budget exists, so an
    // unbounded one aborts the process with SIGABRT — no exit code, no outcome, no audit line.
    // `Interpreter::run` documents that a script failure is a script *outcome*; these prove it.
    for script in [
        // `$(( ( ( ... ) ) ))`: ArithParser::parse_primary re-enters the precedence chain.
        format!("echo $(( {}1{} ))", "(".repeat(4_000), ")".repeat(4_000)),
        // `$( $( ... ) )`: convert_part re-enters the whole parser.
        format!("echo {}echo hi{}", "$(".repeat(2_000), ")".repeat(2_000)),
        // `if ... then`: parse_if re-enters parse_program.
        format!(
            "{}echo x{}",
            "if true; then ".repeat(2_000),
            "; fi".repeat(2_000)
        ),
        // The same nesting reached through an index expression.
        format!(
            "echo ${{name[{}echo 1{}]}}",
            "$(".repeat(1_000),
            ")".repeat(1_000)
        ),
    ] {
        let outcome = run(&script);
        assert_eq!(
            outcome.exit_code,
            ExitCode::SYNTAX,
            "{}",
            &script[..script.len().min(60)]
        );
        assert!(
            outcome.output.contains("syntax error"),
            "{}",
            outcome.output
        );
    }
    // Ordinary nesting is untouched by the ceiling.
    assert_eq!(output("echo $(( ((((1 + 1)))) ))"), "2");
    assert_eq!(output("echo $(echo $(echo $(echo deep)))"), "deep");
}

#[test]
fn the_value_byte_ceiling_stops_runaway_string_growth() {
    // Doubling a string is one cheap step and twice the memory, so every ceiling that counts
    // operations leaves memory unbounded: 26 of these lines reach a gigabyte in 250 steps.
    let outcome = run_with(
        "x=aaaaaaaaaaaaaaaa\ni=0\nwhile [ $i -lt 30 ]; do x=\"$x$x\"; i=$(( i + 1 )); done\necho done",
        Limits {
            max_value_bytes: 64 * 1024,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(
        outcome.output.contains("bytes of values"),
        "{}",
        outcome.output
    );
    assert!(!outcome.output.contains("done"), "{}", outcome.output);

    // A named buffer written in a loop is the same amplification through a different door.
    let outcome = run_with(
        "x=aaaaaaaaaaaaaaaa\ni=0\nwhile [ $i -lt 30 ]; do x=\"$x$x\"; echo $x > buf; i=$(( i + 1 )); done",
        Limits {
            max_value_bytes: 64 * 1024,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);

    // A realistic script stays far inside the default ceiling.
    assert_eq!(
        run("x=hello; y=\"$x $x\"; echo $y").exit_code,
        ExitCode::SUCCESS
    );
}

#[test]
fn the_deadline_bounds_slow_capability_calls_not_only_long_scripts() {
    /// An invoker whose calls cost wall clock rather than steps, like a real provider.
    struct Slow;

    impl CapabilityInvoker for Slow {
        fn granted(&self) -> Vec<String> {
            vec!["slow.call".to_owned()]
        }

        fn invoke(&self, _capability: &str, input: Value) -> CapabilityCallResult {
            std::thread::sleep(Duration::from_millis(20));
            CapabilityCallResult::Succeeded(input)
        }
    }

    // Three steps per capability line times the 32-call default budget is 96 steps, so a clock
    // sampled every 128th step was structurally unreachable for the workload that most needs it:
    // a straight-line script of slow calls used to overrun its deadline and report success.
    let script = "slow.call --i 1\n".repeat(32);
    let outcome = Interpreter::new(Limits {
        timeout: Duration::from_millis(60),
        ..Limits::default()
    })
    .run(&script, &Slow);
    assert_eq!(outcome.exit_code, ExitCode::TIMEOUT, "{}", outcome.output);
    assert!(
        outcome.capability_calls < 32,
        "{}",
        outcome.capability_calls
    );
    assert!(outcome.steps < 128, "{}", outcome.steps);
}

#[test]
fn the_wall_clock_deadline_reports_exit_code_124() {
    let outcome = run_with(
        "sleep 30",
        Limits {
            timeout: Duration::from_millis(20),
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::TIMEOUT);
    assert!(outcome.output.contains("deadline"), "{}", outcome.output);
}

#[test]
fn output_ceilings_truncate_keeping_head_and_tail() {
    let outcome = run_with(
        "i=0\nwhile [ $i -lt 60 ]; do echo line-$i; i=$(( i + 1 )); done",
        Limits {
            max_output_lines: 10,
            ..Limits::default()
        },
    );
    assert!(outcome.truncated);
    assert!(outcome.output.starts_with("line-0\n"), "{}", outcome.output);
    assert!(outcome.output.ends_with("line-59"), "{}", outcome.output);
    assert!(
        outcome.output.contains("Output truncated"),
        "{}",
        outcome.output
    );
    // A truncated script still completes; truncation is not a failure.
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
}

#[test]
fn a_single_oversized_line_cannot_bypass_the_byte_ceiling() {
    let outcome = run_with(
        "echo start\nx=$(echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)\necho \"$x$x$x$x\"",
        Limits {
            max_output_bytes: 64,
            max_output_lines: 100_000,
            ..Limits::default()
        },
    );
    assert!(outcome.truncated);
    assert!(outcome.output.len() < 400, "{}", outcome.output);
}

#[test]
fn the_process_environment_never_leaks_into_a_script() {
    // `PATH` is genuinely set in this test process, so reading it back as empty proves isolation
    // rather than merely proving that some arbitrary name is unset. (`std::env::set_var` is unsafe
    // under edition 2024 and this crate forbids `unsafe`; the runner's black-box CLI test sets a
    // real custom variable on a child process to cover that angle too.)
    assert!(
        std::env::var_os("PATH").is_some(),
        "this test is only meaningful when PATH is set in the host process"
    );
    let outcome = run(r#"echo "[$PATH]"; echo "[$HOME]"; echo "[$OPENAI_API_KEY]""#);
    assert_eq!(outcome.output, "[]\n[]\n[]");
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);

    // Only the script's own assignments seed the namespace.
    assert_eq!(run(r#"PATH=mine; echo "[$PATH]""#).output, "[mine]");
}

#[test]
fn a_normal_multi_step_script_fits_comfortably_in_the_defaults() {
    // A realistic 5-to-10-step plan with nested loops must not trip any default ceiling.
    let script = "\
summarize() {
  local total=0
  for item in $@; do
    total=$(( total + item ))
  done
  echo $total
}

results=''
for group in 1 2 3; do
  inner=0
  while [ $inner -lt 3 ]; do
    r=$(echo.echo --group $group --inner $inner)
    echo ${r[group]}-${r[inner]}
    inner=$(( inner + 1 ))
  done
done
summarize 1 2 3 4
cap --list | jq length";
    let outcome = run(script);
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{}", outcome.output);
    assert!(!outcome.truncated, "{}", outcome.output);
    assert_eq!(outcome.capability_calls, 9);
    assert!(outcome.output.contains("1-0"), "{}", outcome.output);
    assert!(outcome.output.contains("3-2"), "{}", outcome.output);
    assert!(outcome.output.contains("10"), "{}", outcome.output);
    assert!(
        outcome.output.trim_end().ends_with('4'),
        "{}",
        outcome.output
    );
}

#[test]
fn a_syntax_error_reports_exit_code_two_without_running_anything() {
    let outcome = run("echo before\nif true; then echo hi");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(
        outcome.output.contains("syntax error"),
        "{}",
        outcome.output
    );
    assert!(!outcome.output.contains("before"), "{}", outcome.output);
}
