use std::{cell::RefCell, time::Duration};

use serde_json::{Value, json};

use crate::{
    CapabilityCallResult, CapabilityDescription, CapabilityInvoker, CommandRun, ExitCode,
    Interpreter, Limits, ScriptOutcome,
};

const PROBE: &str = "probe";

const PROBE_HELP: &str = "Usage: probe <COMMAND>\n\nCommands:\n  upper  Uppercase text\n";

#[derive(Default)]
pub(super) struct Fixture {
    pub(super) calls: RefCell<Vec<(String, Value)>>,
}

impl CapabilityInvoker for Fixture {
    fn granted(&self) -> Vec<String> {
        vec![
            "cli-probe.upper".to_owned(),
            "fixture.object".to_owned(),
            "http-probe.fetch".to_owned(),
            "policy.denied".to_owned(),
            "provider.broken".to_owned(),
            "provider.refused".to_owned(),
        ]
    }

    fn describe(&self, capability: &str) -> Option<CapabilityDescription> {
        (capability == "cli-probe.upper").then(|| CapabilityDescription {
            capability: capability.to_owned(),
            description: "Uppercases its text".to_owned(),
        })
    }

    fn command_words(&self) -> Vec<String> {
        vec![PROBE.to_owned()]
    }

    fn has_command_word(&self, word: &str) -> bool {
        word == PROBE
    }

    fn run_command(&self, word: &str, argv: &[String], stdin: Option<&str>) -> Option<CommandRun> {
        if word != PROBE {
            return None;
        }
        let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
        Some(match argv.as_slice() {
            ["--help"] => CommandRun::Rendered {
                stdout: PROBE_HELP.to_owned(),
                stderr: String::new(),
                status: 0,
            },
            ["upper", "--text", text] => proposal("cli-probe.upper", json!({ "text": text })),
            ["upper", "-"] => match stdin {
                Some(text) => proposal("cli-probe.upper", json!({ "text": text })),
                None => CommandRun::Failed {
                    message: "probe: no input was piped for -".to_owned(),
                },
            },
            ["object", flags @ ..] => match object_from_flags(flags) {
                Some(object) => proposal("fixture.object", object),
                None => CommandRun::Failed {
                    message: "probe: object takes --key value pairs".to_owned(),
                },
            },
            ["fetch"] => proposal("http-probe.fetch", json!({})),
            ["denied"] => proposal("policy.denied", json!({})),
            ["broken"] => proposal("provider.broken", json!({})),
            ["refused"] => proposal("provider.refused", json!({})),
            ["ungranted"] => proposal("nothing.granted", json!({})),
            ["decline"] => CommandRun::Failed {
                message: "probe: declined".to_owned(),
            },
            ["errored"] => CommandRun::Errored {
                message: "could not connect to broker socket".to_owned(),
            },
            ["cancelled"] => CommandRun::Denied {
                reason: "session-cancelled".to_owned(),
            },
            _ => CommandRun::Rendered {
                stdout: String::new(),
                stderr: format!(
                    "error: unrecognized subcommand '{}'\n\nUsage: probe <COMMAND>\n",
                    argv.first().copied().unwrap_or_default()
                ),
                status: 2,
            },
        })
    }

    fn invoke(
        &self,
        capability: &str,
        input: Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        if secret_use.is_some() {
            return crate::secret_use_unsupported();
        }
        self.calls
            .borrow_mut()
            .push((capability.to_owned(), input.clone()));
        match capability {
            "cli-probe.upper" => match input.get("text").and_then(Value::as_str) {
                Some(text) => {
                    CapabilityCallResult::Succeeded(json!({ "text": text.to_uppercase() }))
                }
                None => CapabilityCallResult::Failed {
                    error: "input must be {\"text\": <string>}".to_owned(),
                    detail: None,
                },
            },
            "fixture.object" => CapabilityCallResult::Succeeded(input),
            "policy.denied" => CapabilityCallResult::Denied {
                reason: "exact policy refused this proposal".to_owned(),
            },
            "provider.broken" => CapabilityCallResult::Failed {
                error: "provider trapped".to_owned(),
                detail: None,
            },
            "provider.refused" => CapabilityCallResult::Failed {
                error: "provider-failure".to_owned(),
                detail: Some(dekopon_core::ProviderFailureDetail::new(
                    "upstream-rejected",
                    "the image route refused the request with HTTP 400 (moderation_blocked)",
                )),
            },
            "http-probe.fetch" => CapabilityCallResult::Succeeded(json!({
                "status": 200,
                "bodyText": "alpha\nbeta\nalpha",
            })),
            _ => CapabilityCallResult::NotFound,
        }
    }
}

fn proposal(capability: &str, input: Value) -> CommandRun {
    CommandRun::Proposed {
        capability: capability.to_owned(),
        input,
        secret_use: None,
    }
}

fn object_from_flags(flags: &[&str]) -> Option<Value> {
    let mut object = serde_json::Map::new();
    for pair in flags.chunks(2) {
        let [flag, text] = pair else {
            return None;
        };
        let key = flag.strip_prefix("--")?;
        let value = serde_json::from_str::<Value>(text)
            .unwrap_or_else(|_| Value::String((*text).to_owned()));
        object.insert(key.to_owned(), value);
    }
    Some(Value::Object(object))
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
        output("nosuchcmd; echo $?"),
        "dekopon-shell: nosuchcmd: command not found\n127"
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
    assert_eq!(
        output("for x in $(probe object --a 1 --b 2 --c 3 | jq '[.a,.b,.c]'); do echo $x; done"),
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
    assert_eq!(
        output(
            "for a in 1 2; do for b in 1 2; do echo $a$b; if [ $b -eq 1 ]; then break 2; fi; done; echo inner-done; done\necho after"
        ),
        "11\nafter"
    );
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
    assert_eq!(output("if ! false; then echo neg; fi"), "neg");
    assert_eq!(output("if ! true; then echo no; else echo yes; fi"), "yes");
    assert_eq!(output("! false && echo reached"), "reached");
    assert_eq!(code("! true"), 1);
    assert_eq!(code("! false"), 0);
    assert_eq!(output("if [ ! -z x ]; then echo arg; fi"), "arg");
}

#[test]
fn functions_participate_in_pipelines_in_both_directions() {
    assert_eq!(output("f() { echo hi; }\nf | wc -c"), "2");
    assert_eq!(output("g() { cat; }\necho payload | g"), "payload");
    assert_eq!(
        output("g() { if [ -n \"$1\" ]; then cat; fi; }\necho payload | g yes"),
        "payload"
    );
    assert_eq!(output("f() { echo one; echo two; }\nf | grep one"), "one");
    assert_eq!(output("f() { echo one; echo two; }\nf"), "one\ntwo");
    assert_eq!(output("echo a\nq() { true; }\nq\necho b"), "a\nb");
}

#[test]
fn a_piped_value_survives_every_stage_and_every_statement_that_shares_it() {
    assert_eq!(
        output("g() { cat; cat; }\necho payload | g"),
        "payload\npayload"
    );
    assert_eq!(
        output("g() { true; echo first; cat; }\necho payload | g"),
        "first\npayload"
    );
    assert_eq!(
        output(r#"probe object --a 1 --b two | jq '.b' | cat"#),
        "two"
    );
    assert_eq!(
        output(r#"g() { cat | jq '.a'; cat | jq '.b'; }; probe object --a 1 --b 2 | g"#),
        "1\n2"
    );
    assert_eq!(output("echo ignored | cat <<EOF\nbody\nEOF"), "body");
}

#[test]
fn prefix_assignments_are_transient_and_applied_after_expansion() {
    assert_eq!(
        output(r#"x=old; x=new echo "[$x]"; echo "after=[$x]""#),
        "[old]\nafter=[old]"
    );
    assert_eq!(output(r#"DEBUG=1 true; echo "[$DEBUG]""#), "[]");
    assert_eq!(output(r#"x=kept; echo "[$x]""#), "[kept]");
}

#[test]
fn shift_consumes_positional_parameters() {
    assert_eq!(output(r#"f() { shift; echo "$1"; }; f a b"#), "b");
    assert_eq!(output(r#"f() { shift 2; echo "$1 $#"; }; f a b c"#), "c 1");
    assert_eq!(output(r#"f() { shift 5; echo $?; }; f a"#), "1");
    assert_eq!(
        output(r#"f() { while [ $# -gt 0 ]; do echo $1; shift; done; }; f a b c"#),
        "a\nb\nc"
    );
}

#[test]
fn quoted_all_positional_splits_one_word_per_parameter() {
    assert_eq!(
        output(r#"f() { for a in "$@"; do echo "[$a]"; done; }; f "one two" three"#),
        "[one two]\n[three]"
    );
    assert_eq!(
        output(r#"f() { count() { echo $#; }; count "$@"; }; f a b c"#),
        "3"
    );
    assert_eq!(
        output(r#"f() { count() { echo $#; }; count "$@"; }; f"#),
        "0"
    );
    assert_eq!(output(r#"f() { echo "[$*]"; }; f a b"#), "[a b]");
    assert_eq!(
        output(r#"f() { count() { echo $#; }; count "$*"; }; f a b"#),
        "1"
    );
}

#[test]
fn diagnostics_inside_a_substitution_still_reach_the_output() {
    let outcome = run(r#"v=$(nosuchcmd); echo "v=[$v] status=$?""#);
    assert!(
        outcome.output.contains("nosuchcmd: command not found"),
        "{}",
        outcome.output
    );
    assert!(
        outcome.output.contains("v=[] status=127"),
        "{}",
        outcome.output
    );

    let outcome = run(r#"v=$(probe denied); echo "[$v]""#);
    assert!(
        outcome
            .output
            .contains("exact policy refused this proposal"),
        "{}",
        outcome.output
    );
}

#[test]
fn a_capture_drops_a_null_result_the_way_the_output_path_does() {
    assert_eq!(output(r#"x=$(true; echo a); echo "[$x]""#), "[a]");
    assert_eq!(output(r#"x=$(echo a; true); echo "[$x]""#), "[a]");
    assert_eq!(
        output(r#"x=$(echo hi | grep zz; echo a); echo "[$x]""#),
        "[a]"
    );
    assert_eq!(output(r#"x=$(echo a; echo b); echo "[$x]""#), "[a\nb]");
    assert_eq!(output("x=$(echo a; false); echo $?"), "1");
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
    assert_eq!(
        output(r#"r=$(probe object --status 200); echo ${r[status]}"#),
        "200"
    );
    assert_eq!(
        output(r#"r="x$(probe object --status 200)"; echo $r"#),
        r#"x{"status":200}"#
    );
}

#[test]
fn a_capture_honors_the_newline_a_command_suppressed() {
    assert_eq!(
        output(r#"v=$(printf '%s' a; printf '%s' b); echo "$v""#),
        "ab"
    );
    assert_eq!(output(r#"v=$(echo -n a; echo -n b); echo "$v""#), "ab");
    assert_eq!(
        output(r#"v=$(echo a; echo b); echo "$v" | wc -l"#),
        "2".to_owned()
    );
    assert_eq!(output(r#"v=$(printf '%s' a; echo b); echo "$v""#), "ab");
    assert_eq!(
        output(r#"v=$(echo a; printf '%s' b); echo "$v" | wc -l"#),
        "2".to_owned()
    );
}

#[test]
fn indexing_is_backed_by_real_json() {
    assert_eq!(
        output(r#"o=$(probe object --a 1 --b 2); echo ${o[b]}"#),
        "2"
    );
    assert_eq!(
        output(r#"a=$(probe object --x 10 | jq '[.x, 20]'); echo ${a[1]}"#),
        "20"
    );
    assert_eq!(
        output(r#"a=$(probe object --x 10 | jq '[.x]'); echo "[${a[9]}]""#),
        "[]"
    );
}

#[test]
fn unquoted_arrays_expand_element_by_element() {
    assert_eq!(
        output(r#"a=$(probe object --x x --y y | jq '[.x,.y]'); count() { echo $#; }; count $a"#),
        "2"
    );
    assert_eq!(
        output(r#"s="one two"; count() { echo $#; }; count $s"#),
        "1"
    );
}

#[test]
fn pipelines_deliver_structured_values() {
    assert_eq!(output(r#"probe object --a 1 | jq .a"#), "1");
    assert_eq!(output("echo 'a\nb\na' | sort | uniq | wc -l"), "2");
    assert_eq!(
        output("probe fetch | jq -r .bodyText | grep alpha | wc -l"),
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
        r#"probe object --a a --b b | jq '[.a,.b]' | xargs probe upper --text"#,
        &fixture,
    );
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{}", outcome.output);
    assert_eq!(outcome.capability_calls, 3);
    assert_eq!(
        *fixture.calls.borrow(),
        vec![
            ("fixture.object".to_owned(), json!({"a": "a", "b": "b"})),
            ("cli-probe.upper".to_owned(), json!({"text": "a"})),
            ("cli-probe.upper".to_owned(), json!({"text": "b"})),
        ]
    );
    assert_eq!(outcome.output, r#"[{"text":"A"},{"text":"B"}]"#);
}

#[test]
fn a_capability_shaped_word_is_an_ordinary_unknown_command() {
    // A capability is only reachable through its provider's command word; a bare
    // capability-identifier-shaped word is not itself callable, even for a capability this session
    // holds.
    let fixture = Fixture::default();
    let outcome = Interpreter::new(Limits::default()).run(
        "wikipedia_page --title x; echo $?\ncli-probe.upper --text hi; echo $?",
        &fixture,
    );
    assert_eq!(
        outcome.output,
        "dekopon-shell: wikipedia_page: command not found\n127\n\
         dekopon-shell: cli-probe.upper: command not found\n127"
    );
    assert!(
        fixture.calls.borrow().is_empty(),
        "a capability-shaped word invoked a capability"
    );
    assert_eq!(outcome.capability_calls, 0);
}

#[test]
fn a_typed_provider_failure_renders_its_code_and_message_after_the_classification() {
    let outcome = Interpreter::new(Limits::default()).run("probe refused", &Fixture::default());

    assert_eq!(
        outcome.output,
        "provider.refused: failed: provider-failure: upstream-rejected: the image route refused \
         the request with HTTP 400 (moderation_blocked)"
    );
    assert_eq!(outcome.exit_code, ExitCode::FAILURE);
}

#[test]
fn a_failure_without_a_provider_detail_renders_the_classification_alone() {
    let outcome = Interpreter::new(Limits::default()).run("probe broken", &Fixture::default());

    assert_eq!(outcome.output, "provider.broken: failed: provider trapped");
    assert_eq!(outcome.exit_code, ExitCode::FAILURE);
}

#[test]
fn capability_outcomes_map_onto_their_documented_exit_codes() {
    assert_eq!(code("probe upper --text a"), 0);
    assert_eq!(code("probe broken"), 1);
    assert_eq!(code("probe denied"), 126);
    assert_eq!(code("probe ungranted"), 127);
    assert_eq!(code("definitelynotacommand"), 127);
}

#[test]
fn cap_lists_and_describes_capabilities() {
    assert!(output("cap --list").contains("cli-probe.upper"));
    let described = output("cap --describe cli-probe.upper");
    assert!(described.contains("Uppercases its text"), "{described}");
    assert!(!described.contains("inputSchema"), "{described}");
}

#[test]
fn a_function_shadows_a_builtin_only_when_declared_first() {
    assert_eq!(output("echo hi"), "hi");
    assert_eq!(output("echo() { true; }\necho hi"), "");
}

#[test]
fn globbing_is_dropped_and_stays_literal() {
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
fn subshells_here_strings_and_process_substitution_are_rejected() {
    for (script, expected) in [
        ("(echo hi)", "subshells"),
        ("cat <<<\"$x\"", "here-string"),
        ("diff <(echo a) b", "process substitution"),
        ("cat < file", "input redirection"),
        ("case $x in a) echo a;& b) echo b;; esac", "falls through"),
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
fn case_runs_the_first_matching_clause_and_only_that_one() {
    let script = "\
for name in ready failed other; do\n\
  case $name in\n\
    ready) echo go ;;\n\
    failed|broken) echo stop ;;\n\
    *) echo unknown ;;\n\
  esac\n\
done";
    assert_eq!(output(script), "go\nstop\nunknown");

    assert_eq!(
        output("case broken in\n ready) echo a ;;\n failed|broken) echo b ;;\n *) echo c ;;\nesac"),
        "b"
    );
}

#[test]
fn case_matches_the_expanded_subject_and_reports_success_when_nothing_matches() {
    assert_eq!(
        output("x=ready\ncase \"$x\" in ready) echo yes ;; esac"),
        "yes"
    );
    let outcome = run("case nothing in ready) echo yes ;; esac");
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
    assert_eq!(outcome.output, "");
}

#[test]
fn an_escaped_case_pattern_matches_one_literal_character_like_bash() {
    let outcome = run("case hello in \\*) echo caught ;; esac");
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
    assert_eq!(outcome.output, "");

    assert_eq!(output("case '*' in \\*) echo star ;; esac"), "star");
    assert_eq!(output("case 'a*b' in a\\*b) echo mid ;; esac"), "mid");
    assert_eq!(output("case '?' in \\?) echo mark ;; esac"), "mark");
}

#[test]
fn case_composes_with_the_control_flow_around_it() {
    assert_eq!(
        output("for n in 1 2 3; do case $n in 2) break ;; *) echo $n ;; esac; done"),
        "1"
    );
    assert_eq!(
        output("f() { case $1 in a) return 0 ;; *) return 1 ;; esac; }\nf a && echo matched"),
        "matched"
    );
}

#[test]
fn a_case_pattern_assembled_at_run_time_is_still_checked() {
    let outcome = run("p='*.json'\ncase report.json in $p) echo matched ;; esac");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(
        outcome.output.contains("expanded to text"),
        "{}",
        outcome.output
    );
    // An error here must not suggest quoting: quoting only exempts a metacharacter while the parser
    // can still see it, and an expanded pattern has already lost its quotes.
    assert!(
        !outcome.output.contains("quote it as"),
        "{}",
        outcome.output
    );

    assert_eq!(
        output("p=ready\ncase ready in $p) echo matched ;; esac"),
        "matched"
    );
}

#[test]
fn case_charges_the_step_budget_like_every_other_construct() {
    let outcome = run_with(
        "while true; do case x in a) : ;; b) : ;; *) : ;; esac; done",
        Limits {
            max_steps: 200,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(outcome.output.contains("step budget"), "{}", outcome.output);
}

#[test]
fn a_here_document_becomes_the_commands_input_as_one_string() {
    assert_eq!(output("cat <<EOF\nalpha\nbeta\nEOF"), "alpha\nbeta");

    assert_eq!(
        output("jq -r 'fromjson.name' <<EOF\n{\"name\": \"dekopon\"}\nEOF"),
        "dekopon"
    );
    let unparsed = run("jq -r .name <<EOF\n{\"name\": \"dekopon\"}\nEOF");
    assert_eq!(unparsed.exit_code, ExitCode::FAILURE);
    assert!(
        unparsed.output.contains("cannot index"),
        "{}",
        unparsed.output
    );
}

#[test]
fn a_here_document_interpolates_unless_its_delimiter_is_quoted() {
    assert_eq!(output("id=7\ncat <<EOF\nid=$id\nEOF"), "id=7");
    assert_eq!(output("id=7\ncat <<'EOF'\nid=$id\nEOF"), "id=$id");
    assert_eq!(output("cat <<EOF\nvalue=$(echo inner)\nEOF"), "value=inner");
}

#[test]
fn a_here_document_replaces_what_a_pipe_would_have_supplied() {
    assert_eq!(
        output("echo piped | cat <<EOF\nredirected\nEOF"),
        "redirected"
    );
    assert_eq!(output("cat <<EOF | wc -l\na\nb\nEOF"), "2");
}

#[test]
fn a_here_document_body_charges_the_value_byte_ceiling() {
    let body = "x".repeat(4096);
    let outcome = run_with(
        &format!("cat <<EOF\n{body}\nEOF"),
        Limits {
            max_value_bytes: 512,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(
        outcome.output.contains("bytes of values"),
        "{}",
        outcome.output
    );
}

#[test]
fn the_clock_is_not_a_command_this_shell_has() {
    let outcome = run("date");
    assert_eq!(outcome.exit_code, ExitCode::NOT_FOUND);
    assert!(
        outcome.output.contains("command not found"),
        "{}",
        outcome.output
    );
}

#[test]
fn array_expansion_is_backed_by_real_json() {
    assert_eq!(
        output(
            r#"arr=$(probe object --a x --b y | jq '[.a,.b]')
for item in "${arr[@]}"; do echo "[$item]"; done"#
        ),
        "[x]\n[y]"
    );
    assert_eq!(
        output(
            r#"arr=$(probe object --a x --b y | jq '[.a,.b]')
echo "${arr[*]}""#
        ),
        "x y"
    );
    assert_eq!(
        output(
            r#"arr=$(probe object --a x --b y | jq '[.a,.b]')
echo ${#arr[@]}"#
        ),
        "2"
    );
    assert_eq!(
        output(
            r#"arr=$(probe object --a "one two" | jq '[.a]')
for item in "${arr[@]}"; do echo "[$item]"; done"#
        ),
        "[one two]"
    );
}

#[test]
fn while_read_walks_every_line_and_then_stops() {
    assert_eq!(
        output(r#"probe fetch | jq -r .bodyText | while read line; do echo "[$line]"; done"#),
        "[alpha]\n[beta]\n[alpha]"
    );
    assert_eq!(
        output(
            r#"count=0
probe fetch | jq -r .bodyText | while read line; do count=$(( count + 1 )); done
echo $count"#
        ),
        "3"
    );
}

#[test]
fn read_reports_end_of_input_as_a_status_not_a_diagnostic() {
    let outcome = run("echo one | while read line; do echo $line; done");
    assert_eq!(outcome.output, "one");
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
    assert_eq!(code("echo '' | read x"), 1, "no lines is a failing read");
}

#[test]
fn read_binds_several_names_by_splitting_on_whitespace() {
    assert_eq!(
        output(
            r#"echo "alpha beta gamma delta" | { read -r first second rest; echo "1=$first 2=$second rest=$rest"; }"#
        ),
        "1=alpha 2=beta rest=gamma delta"
    );
    assert_eq!(
        output(r#"echo "only" | { read -r a b; echo "[$a][$b]"; }"#),
        "[only][]"
    );
}

#[test]
fn a_piped_read_is_its_own_one_shot_source() {
    assert_eq!(output("echo hello | read x\necho $x"), "hello");
}

#[test]
fn read_refuses_what_it_does_not_implement() {
    for (script, expected) in [
        ("echo a | read", "needs at least one variable name"),
        ("echo a | read -d ,", "option \"-d\" is not supported"),
        ("echo a | read 1bad", "is not a valid variable name"),
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
fn errexit_ends_the_script_at_the_first_untested_failure() {
    let outcome = run("set -e\necho before\nnosuchcmd\necho after");
    assert_eq!(outcome.exit_code.get(), 127);
    assert!(outcome.output.contains("before"), "{outcome:?}");
    assert!(!outcome.output.contains("after"), "{outcome:?}");
    assert!(outcome.output.contains("`set -e` is on"), "{outcome:?}");

    assert!(output("nosuchcmd\necho after").contains("after"));
    assert!(
        output("set -e\nset +e\nnosuchcmd\necho after").contains("after"),
        "`set +e` must restore the default"
    );
}

#[test]
fn errexit_leaves_a_tested_status_alone() {
    for script in [
        "set -e\nif nosuchcmd; then echo yes; else echo handled; fi\necho after",
        "set -e\nnosuchcmd || echo handled\necho after",
        "set -e\nif true && nosuchcmd; then echo handled; else echo handled; fi\necho after",
    ] {
        let outcome = run(script);
        assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{script}");
        assert!(
            outcome.output.contains("handled"),
            "{script}: {}",
            outcome.output
        );
        assert!(
            outcome.output.contains("after"),
            "{script}: {}",
            outcome.output
        );
    }
    assert_eq!(code("set -e\n! nosuchcmd\necho after"), 0);
    assert_eq!(
        code("set -e\nwhile nosuchcmd; do echo body; done\necho after"),
        0
    );
    let outcome = run("set -e\ntrue && nosuchcmd\necho after");
    assert_eq!(outcome.exit_code.get(), 127);
    assert!(!outcome.output.contains("after"), "{outcome:?}");
}

#[test]
fn nounset_refuses_a_name_nothing_ever_set() {
    let outcome = run("set -u\necho \"[$missing]\"\necho after");
    assert_eq!(outcome.exit_code, ExitCode::FAILURE);
    assert!(
        outcome.output.contains("missing: unbound variable"),
        "{outcome:?}"
    );
    assert!(!outcome.output.contains("after"), "{outcome:?}");

    assert_eq!(output("set -u\necho ${missing:-fallback}"), "fallback");
    assert_eq!(output("set -u\necho \"[${missing+set}]\""), "[]");
    assert_eq!(output("set -u\nx=\necho \"[$x]\""), "[]");
}

#[test]
fn pipefail_reports_the_rightmost_stage_that_failed() {
    assert_eq!(code("nosuchcmd | jq ."), 0);
    assert_eq!(code("set -o pipefail\nnosuchcmd | jq ."), 127);
    assert_eq!(code("set -o pipefail\necho hi | jq ."), 0);
    assert_eq!(
        code("set -o pipefail\nset +o pipefail\nnosuchcmd | jq ."),
        0
    );
}

#[test]
fn pipestatus_reports_every_stage() {
    assert!(output("nosuchcmd | jq .\necho ${PIPESTATUS[@]}").ends_with("127 0"));
    assert_eq!(output("echo hi\necho ${PIPESTATUS[0]}"), "hi\n0");
    assert_eq!(
        output("echo a | jq . | wc -l\necho ${#PIPESTATUS[@]}"),
        "1\n3"
    );
}

#[test]
fn set_refuses_every_option_it_does_not_enforce() {
    for (script, expected) in [
        ("set", "listing or setting positional parameters"),
        ("set -x", "option -x is not supported"),
        ("set -o", "-o needs an option name"),
        ("set -o noclobber", "-o noclobber is not supported"),
        ("set --", "sets positional parameters"),
        ("set nope", "is not an option"),
    ] {
        let outcome = run(script);
        assert_eq!(outcome.exit_code, ExitCode::SYNTAX, "{script}");
        assert!(
            outcome.output.contains(expected),
            "{script}: {}",
            outcome.output
        );
    }
    assert_eq!(code("set -o errexit\nnosuchcmd"), 127);
    assert_eq!(code("set -o nounset\necho $missing"), 1);
}

#[test]
fn double_brackets_run_the_same_tests_single_ones_do() {
    for (double, single) in [
        ("[[ -n x ]]", "[ -n x ]"),
        ("[[ -z \"\" ]]", "[ -z \"\" ]"),
        ("[[ a = a ]]", "[ a = a ]"),
        ("[[ a != b ]]", "[ a != b ]"),
        ("[[ 2 -lt 10 ]]", "[ 2 -lt 10 ]"),
        ("[[ ! -n \"\" ]]", "[ ! -n \"\" ]"),
    ] {
        assert_eq!(code(double), code(single), "{double} vs {single}");
        assert_eq!(code(double), 0, "{double}");
    }
    assert_eq!(code("[[ -n \"\" ]]"), 1);
}

#[test]
fn double_brackets_add_the_connectives_single_ones_lack() {
    assert_eq!(output("[[ -n a && -n b ]] && echo both"), "both");
    assert_eq!(output("[[ -z a || -n b ]] && echo either"), "either");
    assert_eq!(
        output("[[ ! ( -n a && -z b ) ]] && echo grouped"),
        "grouped"
    );
    assert_eq!(
        output("x=5\n[[ $x -gt 1 && $x -lt 10 ]] && echo between"),
        "between"
    );
    assert_eq!(
        output("[[ -n \"$missing\" && $missing -eq 1 ]] || echo skipped"),
        "skipped"
    );
    assert_eq!(output("if [[ -n x ]]; then echo yes; fi"), "yes");
    assert_eq!(
        output("i=0\nwhile [[ $i -lt 2 ]]; do echo $i; i=$(( i + 1 )); done"),
        "0\n1"
    );
}

#[test]
fn an_unquoted_expansion_inside_double_brackets_is_one_word() {
    assert_eq!(
        output(
            r#"v=$(probe object --a "one two" | jq '[.a]')
[[ -n $v ]] && echo held"#
        ),
        "held"
    );
}

#[test]
fn comparison_operands_inside_double_brackets_stay_literal() {
    let outcome = run("f=report.json\n[[ $f == *.json ]] && echo matched");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(outcome.output.contains("glob in bash"), "{outcome:?}");
    assert!(!outcome.output.contains("matched"), "{outcome:?}");

    assert_eq!(output("f='*'\n[[ $f == '*' ]] && echo literal"), "literal");
    let outcome = run("p='*.json'\nf=report.json\n[[ $f == $p ]] && echo matched");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(
        outcome.output.contains("quoting cannot exempt"),
        "{outcome:?}"
    );

    let outcome = run("[[ abc =~ a.c ]]");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(outcome.output.contains("regex matching"), "{outcome:?}");
}

#[test]
fn a_malformed_double_bracket_condition_names_what_is_wrong() {
    assert!(run("[[ -n x ").output.contains("expected `]]`"));
    assert!(run("[[ ]]").output.contains("expected a condition"));
    assert!(
        run("[[ a b c d ]]")
            .output
            .contains("at most three operands")
    );
    assert!(run("[[ ( -n x ]]").output.contains("expected `)`"));
    assert!(run("[[ -f x ]]").output.contains("no filesystem"));
}

#[test]
fn a_compound_command_can_be_a_pipeline_stage() {
    assert_eq!(
        output(
            r#"probe object --a 1 | while [ -n "$(cat | jq -r .a)" ]; do echo saw; break; done"#
        ),
        "saw"
    );
    assert_eq!(
        output(
            "probe object --a 1 | if [ $(cat | jq -r .a) -eq 1 ]; then echo yes; else echo no; fi"
        ),
        "yes"
    );
    assert_eq!(
        output(
            "probe object --a 1 --b 2 | case $(cat | jq -r .a) in 1) echo one ;; *) echo other ;; esac"
        ),
        "one"
    );
}

#[test]
fn a_piped_compound_stage_keeps_the_variables_it_assigns() {
    assert_eq!(
        output(
            r#"total=0
probe object --a 7 | while [ $total -eq 0 ]; do total=$(cat | jq -r .a); done
echo $total"#
        ),
        "7"
    );
}

#[test]
fn a_compound_stage_feeding_a_pipe_collects_everything_it_emitted() {
    assert_eq!(output("{ echo a; echo b; } | wc -l"), "2");
    assert_eq!(output("for x in 1 2 3; do echo $x; done | wc -l"), "3");
    assert_eq!(output("{ echo a; echo b; } > buf\ncat buf | wc -l"), "2");
}

#[test]
fn a_brace_group_runs_in_the_current_scope_and_is_one_branch() {
    let outcome = run("nosuchcmd || { echo handled; exit 3; }\necho unreachable");
    assert_eq!(outcome.exit_code.get(), 3);
    assert!(outcome.output.contains("handled"), "{outcome:?}");
    assert!(!outcome.output.contains("unreachable"), "{outcome:?}");

    assert_eq!(output("{ x=inside; }\necho $x"), "inside");
    assert_eq!(output("{ true; false; } && echo yes || echo no"), "no");
}

#[test]
fn an_empty_or_unterminated_group_is_a_parse_error_naming_itself() {
    assert!(run("{ }").output.contains("empty `{ }` group"));
    assert!(run("{ echo hi").output.contains("expected `}`"));
}

#[test]
fn a_compound_stage_carries_its_own_redirections() {
    let outcome = run("{ nosuchone; nosuchtwo; } 2> log\necho ---\ncat log | wc -l");
    let (before, after) = outcome.output.split_once("---").expect("the marker");
    assert!(!before.contains("command not found"), "{before:?}");
    assert_eq!(after.trim(), "2");
}

#[test]
fn default_and_alternate_expansions_follow_bash_including_the_colon() {
    assert_eq!(output("echo ${missing:-fallback}"), "fallback");
    assert_eq!(output("x=set\necho ${x:-fallback}"), "set");
    assert_eq!(output("x=\necho ${x:-fallback}"), "fallback");
    assert_eq!(output("x=\necho \"[${x-fallback}]\""), "[]");

    assert_eq!(output("x=set\necho ${x:+present}"), "present");
    assert_eq!(output("echo \"[${missing:+present}]\""), "[]");

    assert_eq!(output("y=inner\necho ${x:-$y}"), "inner");
    assert_eq!(
        output("v=${x:-$(probe object --a 1)}\necho ${v[a]}"),
        "1",
        "a bare substitution default keeps its structure"
    );
}

#[test]
fn a_whole_right_hand_side_expansion_keeps_the_value_it_names() {
    assert_eq!(
        output("obj=$(probe object --a 1)\ncopy=$obj\necho ${copy[a]}"),
        "1"
    );
    assert_eq!(
        output("obj=$(probe object --a 1)\njoined=x$obj\necho $joined"),
        r#"x{"a":1}"#
    );
}

#[test]
fn assign_expansion_binds_the_name_it_substituted_for() {
    assert_eq!(output("echo ${x:=first}\necho $x"), "first\nfirst");
    assert_eq!(output("x=kept\necho ${x:=other}\necho $x"), "kept\nkept");
    let outcome = run("obj=$(probe object --a 1)\necho ${obj[b]:=x}");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(outcome.output.contains("cannot assign through an index"));
}

#[test]
fn a_required_expansion_ends_the_script_rather_than_carrying_on_empty() {
    let outcome = run("echo ${token:?no credential in scope}\necho after");
    assert_eq!(outcome.exit_code, ExitCode::FAILURE);
    assert!(
        outcome.output.contains("token: no credential in scope"),
        "{outcome:?}"
    );
    assert!(!outcome.output.contains("after"), "{outcome:?}");

    assert_eq!(output("token=ok\necho ${token:?missing}"), "ok");
    assert!(
        run("echo ${token:?}")
            .output
            .contains("token: parameter is not set")
    );
}

#[test]
fn length_counts_what_the_value_actually_is() {
    assert_eq!(output("x=hello\necho ${#x}"), "5");
    assert_eq!(output("echo ${#missing}"), "0");
    assert_eq!(output("obj=$(probe object --a 1 --b 2)\necho ${#obj}"), "2");
    assert_eq!(output("x=héllo\necho ${#x}"), "5");
}

#[test]
fn prefix_suffix_and_replacement_operate_on_literal_text() {
    assert_eq!(output("p=owner/repo\necho ${p#owner/}"), "repo");
    assert_eq!(output("p=owner/repo\necho ${p%/repo}"), "owner");
    assert_eq!(output("p=owner/repo\necho ${p#nope}"), "owner/repo");
    assert_eq!(output("p=owner/repo\necho ${p##owner/}"), "repo");
    assert_eq!(output("p=owner/repo\necho ${p%%/repo}"), "owner");

    assert_eq!(output("p=a-b-c\necho ${p/-/+}"), "a+b-c");
    assert_eq!(output("p=a-b-c\necho ${p//-/+}"), "a+b+c");
    assert_eq!(output("p=a-b\necho ${p//-}"), "ab");
}

#[test]
fn a_metacharacter_in_an_expansion_pattern_is_rejected_rather_than_matched_literally() {
    for script in ["p=a/b\necho ${p##*/}", "p=a.json\necho ${p%.*}"] {
        let outcome = run(script);
        assert_eq!(outcome.exit_code, ExitCode::SYNTAX, "{script}");
        assert!(outcome.output.contains("literal text"), "{script}");
    }
    let outcome = run("star='*'\np=a.json\necho ${p%$star}");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(
        outcome.output.contains("quoting cannot exempt"),
        "{outcome:?}"
    );
    assert_eq!(output("p='*.json'\necho ${p#'*'}"), ".json");
}

#[test]
fn nested_parameter_expansions_have_a_ceiling_rather_than_a_stack_overflow() {
    let deep = format!("echo {}x{}", "${a:-".repeat(200), "}".repeat(200));
    let outcome = run(&deep);
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(outcome.output.contains("nested deeper"), "{outcome:?}");
}

#[test]
fn a_redirected_stderr_leaves_the_combined_output() {
    let outcome = run("nosuchcmd 2>/dev/null\necho done");
    assert_eq!(outcome.output, "done");
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);

    assert_eq!(code("nosuchcmd 2>/dev/null"), 127);
}

#[test]
fn stderr_redirects_into_a_named_buffer_that_cat_reads_back() {
    let outcome = run("nosuchcmd 2> log\necho ---\ncat log");
    assert!(!outcome.output.starts_with("dekopon-shell:"), "{outcome:?}");
    let (before, after) = outcome.output.split_once("---").expect("the marker");
    assert!(!before.contains("command not found"), "{before:?}");
    assert!(after.contains("command not found"), "{after:?}");
}

#[test]
fn a_value_sent_to_stderr_escapes_a_command_substitution() {
    let outcome = run(r#"x=$(echo oops >&2; echo kept)
echo "[$x]""#);
    assert!(outcome.output.contains("oops"), "{outcome:?}");
    assert!(outcome.output.contains("[kept]"), "{outcome:?}");
}

#[test]
fn two_to_one_merges_diagnostics_into_the_value_a_substitution_captures() {
    let outcome = run(r#"x=$(nosuchcmd 2>&1)
echo "[$x]""#);
    assert!(outcome.output.contains("command not found]"), "{outcome:?}");
}

#[test]
fn two_to_one_leaves_a_quiet_command_s_value_and_its_type_alone() {
    // 2>&1 merges output only when there is an actual diagnostic to merge; otherwise a structured
    // value must not be flattened into its own JSON text.
    assert_eq!(output("echo hi 2>&1"), "hi");
    assert_eq!(
        output("probe object --a 1 2>&1 | jq -r .a"),
        "1",
        "a quiet provider command keeps its object"
    );
}

#[test]
fn both_streams_can_land_in_one_buffer() {
    let outcome = run("nosuchcmd > out 2>&1\ncat out");
    assert!(outcome.output.contains("command not found"), "{outcome:?}");

    let outcome = run("echo hi &> all\ncat all");
    assert_eq!(outcome.output, "hi");
}

#[test]
fn dev_null_discards_on_write_and_reads_empty() {
    assert_eq!(output("echo hi > /dev/null\necho after"), "after");
    assert_eq!(output("cat /dev/null"), "");
    assert!(output("cat nosuchbuffer").contains("no such buffer"));
}

#[test]
fn a_redirection_covers_the_whole_body_of_the_function_it_is_written_on() {
    let outcome = run(
        "noisy() { nosuchone; nosuchtwo; echo value; }\nnoisy 2> log\necho ---\ncat log | wc -l",
    );
    let (before, after) = outcome.output.split_once("---").expect("the marker");
    assert!(!before.contains("command not found"), "{before:?}");
    assert_eq!(after.trim(), "2", "both diagnostics were collected");
}

#[test]
fn a_fatal_diagnostic_is_never_swallowed_by_a_redirection() {
    // A fatal diagnostic must not be swallowed by a stderr redirect, or a model would see an empty
    // result with no explanation.
    let outcome = run_with(
        "loop() { loop; }\nloop 2>/dev/null",
        Limits {
            max_recursion_depth: 4,
            ..Limits::default()
        },
    );
    assert!(outcome.output.contains("nested deeper"), "{outcome:?}");
}

#[test]
fn a_redirection_target_still_has_to_be_one_word() {
    let outcome = run("x=$(probe object --a one --b two | jq '[.a,.b]')\necho hi > $x");
    assert_ne!(outcome.exit_code, ExitCode::SUCCESS);
    assert!(
        outcome.output.contains("exactly one buffer name"),
        "{outcome:?}"
    );
}

#[test]
fn shell_shapes_this_interpreter_cannot_honor_are_rejected_by_their_own_name() {
    for (script, expected) in [
        ("echo `echo hi`", "backtick command substitution"),
        ("x=`date`", "backtick command substitution"),
        ("echo hi 3>/dev/null", "only descriptors 1"),
        ("cat 0< buf", "input duplication"),
        ("set -x\necho after", "option -x is not supported"),
        (
            "set -o noclobber\necho after",
            "-o noclobber is not supported",
        ),
        ("i=0; ((i++))", "arithmetic command"),
        ("arr=(a b c)", "bash array literals"),
        ("for ((i=0; i<3; i++)); do echo $i; done", "C-style"),
        ("(echo hi)", "subshells"),
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
    let outcome = run("echo $(( 1 é 2 ))");
    assert!(outcome.output.contains("'é'"), "{}", outcome.output);
}

#[test]
fn a_text_builtin_that_selected_nothing_emits_nothing() {
    assert_eq!(
        output("echo start; echo a | grep zzz; echo end"),
        "start\nend"
    );
    assert_eq!(
        output("echo start; echo a | grep zzz | wc -l; echo end"),
        "start\n0\nend"
    );
}

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
        "for i in 1 2 3 4 5; do probe upper --text $i; done",
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
    for script in [
        format!("echo $(( {}1{} ))", "(".repeat(4_000), ")".repeat(4_000)),
        format!("echo {}echo hi{}", "$(".repeat(2_000), ")".repeat(2_000)),
        format!(
            "{}echo x{}",
            "if true; then ".repeat(2_000),
            "; fi".repeat(2_000)
        ),
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
    assert_eq!(output("echo $(( ((((1 + 1)))) ))"), "2");
    assert_eq!(output("echo $(echo $(echo $(echo deep)))"), "deep");
}

#[test]
fn the_value_byte_ceiling_stops_runaway_string_growth() {
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

    let outcome = run_with(
        "x=aaaaaaaaaaaaaaaa\ni=0\nwhile [ $i -lt 30 ]; do x=\"$x$x\"; echo $x > buf; i=$(( i + 1 )); done",
        Limits {
            max_value_bytes: 64 * 1024,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);

    assert_eq!(
        run("x=hello; y=\"$x $x\"; echo $y").exit_code,
        ExitCode::SUCCESS
    );
}

#[test]
fn the_deadline_bounds_slow_capability_calls_not_only_long_scripts() {
    struct Slow;

    impl CapabilityInvoker for Slow {
        fn granted(&self) -> Vec<String> {
            vec!["slow.call".to_owned()]
        }

        fn has_command_word(&self, word: &str) -> bool {
            word == "slow"
        }

        fn run_command(
            &self,
            word: &str,
            _argv: &[String],
            _stdin: Option<&str>,
        ) -> Option<CommandRun> {
            (word == "slow").then(|| proposal("slow.call", json!({})))
        }

        fn invoke(
            &self,
            _capability: &str,
            input: Value,
            _secret_use: Option<dekopon_core::SecretUseProposal>,
        ) -> CapabilityCallResult {
            std::thread::sleep(Duration::from_millis(20));
            CapabilityCallResult::Succeeded(input)
        }
    }

    let script = "slow call\n".repeat(32);
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
fn a_provider_command_help_page_is_stdout_exit_0_and_capturable() {
    let fixture = Fixture::default();
    let outcome = Interpreter::new(Limits::default()).run("probe --help", &fixture);
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
    assert_eq!(
        outcome.output,
        "Usage: probe <COMMAND>\n\nCommands:\n  upper  Uppercase text"
    );
    assert_eq!(outcome.capability_calls, 0);
    assert!(
        fixture.calls.borrow().is_empty(),
        "help invoked a capability"
    );

    let captured = run("h=$(probe --help); echo \"[$?]\"; echo \"$h\"");
    assert_eq!(
        captured.output,
        "[0]\nUsage: probe <COMMAND>\n\nCommands:\n  upper  Uppercase text"
    );
    assert_eq!(captured.capability_calls, 0);
}

#[test]
fn a_provider_command_usage_error_is_a_diagnostic_with_exit_2() {
    let outcome = run("x=$(probe bogus); echo \"[$x] $?\"");
    assert_eq!(
        outcome.output,
        "error: unrecognized subcommand 'bogus'\n\nUsage: probe <COMMAND>\n[] 2"
    );
    assert_eq!(outcome.capability_calls, 0);
    assert_eq!(
        output("probe bogus 2> log; echo \"status $?\"; cat log"),
        "status 2\nerror: unrecognized subcommand 'bogus'\n\nUsage: probe <COMMAND>"
    );
}

#[test]
fn a_provider_command_reads_piped_text_verbatim_and_values_as_json() {
    let fixture = Fixture::default();
    let outcome = Interpreter::new(Limits::default()).run(
        "echo hello | probe upper -\nprobe object --a 1 | probe upper -\nprobe upper --text flag",
        &fixture,
    );
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{}", outcome.output);
    assert_eq!(
        *fixture.calls.borrow(),
        vec![
            ("cli-probe.upper".to_owned(), json!({"text": "hello"})),
            ("fixture.object".to_owned(), json!({"a": 1})),
            ("cli-probe.upper".to_owned(), json!({"text": "{\"a\":1}"})),
            ("cli-probe.upper".to_owned(), json!({"text": "flag"})),
        ]
    );
    assert_eq!(outcome.capability_calls, 4);
    assert_eq!(
        output("probe upper -; echo $?"),
        "probe: no input was piped for -\n2"
    );
}

#[test]
fn a_provider_command_proposal_still_needs_the_grant() {
    let fixture = Fixture::default();
    let outcome = Interpreter::new(Limits::default()).run("probe ungranted", &fixture);
    assert_eq!(outcome.exit_code, ExitCode::NOT_FOUND);
    assert_eq!(
        outcome.output,
        "dekopon-shell: probe: requires capability nothing.granted, which is not granted in this \
         session"
    );
    assert!(
        fixture.calls.borrow().is_empty(),
        "an ungranted proposal ran"
    );
    assert_eq!(outcome.capability_calls, 0);
}

#[test]
fn a_provider_command_decline_is_a_usage_error() {
    let outcome = run("probe decline");
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert_eq!(outcome.output, "probe: declined");
    assert_eq!(outcome.capability_calls, 0);
}

#[test]
fn a_command_run_that_errored_or_was_refused_is_not_a_usage_error() {
    let errored = run("probe errored; echo $?");
    assert_eq!(
        errored.output,
        "probe: failed: could not connect to broker socket\n1"
    );
    assert_eq!(errored.capability_calls, 0);

    let refused = run("probe cancelled; echo $?");
    assert_eq!(refused.output, "probe: denied: session-cancelled\n126");
    assert_eq!(refused.capability_calls, 0);
}

#[test]
fn a_rendered_page_charges_no_capability_call() {
    let outcome = run_with(
        "probe --help > page\nprobe --help > page\nprobe upper --text once\necho $?",
        Limits {
            max_capability_calls: 1,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{}", outcome.output);
    assert_eq!(outcome.output, "{\"text\":\"ONCE\"}\n0");
    assert_eq!(outcome.capability_calls, 1);
}

#[test]
fn rendered_output_obeys_the_output_ceiling() {
    let outcome = run_with(
        "probe --help",
        Limits {
            max_output_bytes: 16,
            max_output_lines: 100_000,
            ..Limits::default()
        },
    );
    assert!(outcome.truncated, "{}", outcome.output);
    assert!(
        outcome.output.contains("Output truncated"),
        "{}",
        outcome.output
    );
    let outcome = run_with(
        "probe --help",
        Limits {
            max_value_bytes: 16,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX, "{}", outcome.output);
    assert!(
        outcome.output.contains("bytes of values"),
        "{}",
        outcome.output
    );
}

#[test]
fn the_process_environment_never_leaks_into_a_script() {
    assert!(
        std::env::var_os("PATH").is_some(),
        "this test is only meaningful when PATH is set in the host process"
    );
    let outcome = run(r#"echo "[$PATH]"; echo "[$HOME]"; echo "[$OPENAI_API_KEY]""#);
    assert_eq!(outcome.output, "[]\n[]\n[]");
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);

    assert_eq!(run(r#"PATH=mine; echo "[$PATH]""#).output, "[mine]");
}

#[test]
fn a_normal_multi_step_script_fits_comfortably_in_the_defaults() {
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
    r=$(probe object --group $group --inner $inner)
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
        outcome.output.trim_end().ends_with('6'),
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
