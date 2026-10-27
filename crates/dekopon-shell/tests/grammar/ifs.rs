// Corpus exclusions:
// - Bare subshells, Bash array literals, here-strings and process substitution: rejected parser syntax; use supported substitutions, JSON arrays and input pipes.
// - ANSI-C quoting, indirect expansion, sparse/associative arrays and unsupported parameter operators: outside the accepted language.
// - printf -v, mapfile, unsupported read flags and unrelated OS commands: absent builtins/options; fixtures use the supported subset.
// - Input NUL and non-UTF-8 bytes: shell values are UTF-8 text; malformed Bash output remains covered by explicit refusal assertions.
// - Locales other than C.UTF-8: the oracle fixes this locale to compare UTF-8 delimiter behavior deterministically.
// - Filesystem pathname expansion: this interpreter has no filesystem globbing.

use super::*;
use std::{
    io::Read,
    process::{Command, Stdio},
    time::Instant,
};

fn bash(script: &str) -> Vec<Vec<Vec<u8>>> {
    let prefix = r#"[[ ${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]} == 5.2 ]] || exit 98
shopt -s lastpipe
set +m
probe() { [[ $1 == args ]] || return 99; shift; printf '%s\0' "$#"; for arg in "$@"; do printf '%s\0' "$arg"; done; }
"#;
    let mut child = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &format!("{prefix}{script}")])
        .env("LC_ALL", "C.UTF-8")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("real Bash 5.2 must be available on PATH");
    let deadline = Instant::now() + Duration::from_secs(10);
    let stdout = child.stdout.take().expect("Bash stdout");
    let stderr = child.stderr.take().expect("Bash stderr");
    let (stdout, stderr, status) = std::thread::scope(|scope| {
        let collect = |stream: Box<dyn Read + Send>| {
            let mut bytes = Vec::new();
            stream
                .take(64 * 1024)
                .read_to_end(&mut bytes)
                .expect("Bash output");
            bytes
        };
        let out = scope.spawn(move || collect(Box::new(stdout)));
        let err = scope.spawn(move || collect(Box::new(stderr)));
        let status = loop {
            if let Some(status) = child.try_wait().expect("Bash status") {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().expect("kill the owned Bash process");
                child.wait().expect("reap Bash");
                panic!("Bash oracle exceeded its deadline: {script}");
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        (
            out.join().expect("stdout reader"),
            err.join().expect("stderr reader"),
            status,
        )
    });
    assert!(
        status.success(),
        "Bash 5.2 oracle status {status}: {script}: {stderr:?}"
    );
    assert!(stderr.is_empty(), "Bash diagnostic: {script}: {stderr:?}");
    assert!(stdout.len() < 64 * 1024, "bounded oracle output");
    let mut parts = stdout.split(|byte| *byte == 0);
    let mut calls = Vec::new();
    while let Some(count) = parts.next().filter(|part| !part.is_empty()) {
        let count: usize = std::str::from_utf8(count)
            .expect("argument count")
            .parse()
            .expect("numeric count");
        calls.push(
            (0..count)
                .map(|_| parts.next().expect("argument bytes").to_vec())
                .collect(),
        );
    }
    calls
}

fn compare(script: &str) {
    compare_pair(script, script, "named regression");
}

fn compare_pair(script: &str, reference: &str, context: &str) -> bool {
    let expected = bash(reference);
    let fixture = Fixture::default();
    let result = Interpreter::new(Limits::default()).run(script, &fixture);
    let actual: Vec<Vec<Vec<u8>>> = fixture
        .calls
        .lock()
        .iter()
        .filter(|(capability, _)| capability == "fixture.argv")
        .map(|(_, args)| {
            args.as_array()
                .expect("argv")
                .iter()
                .map(|arg| arg.as_str().expect("text").as_bytes().to_vec())
                .collect()
        })
        .collect();
    if let Some(first_invalid) = expected
        .iter()
        .position(|call| call.iter().any(|bytes| std::str::from_utf8(bytes).is_err()))
    {
        assert_eq!(
            actual,
            expected[..first_invalid],
            "{context}: refuse before invoking malformed argv"
        );
        assert_eq!(
            result.exit_code,
            ExitCode::SYNTAX,
            "{context}: {script}: {result:?}"
        );
        assert_eq!(
            result.output, "dekopon-shell: IFS: Bash would produce a field that is not valid UTF-8",
            "{context}: {script}"
        );
        eprintln!("{context}: explicit UTF-8 refusal; raw Bash={expected:?}");
        true
    } else {
        assert_eq!(
            result.exit_code,
            ExitCode::SUCCESS,
            "{context}: {script}: {result:?}"
        );
        assert_eq!(
            actual, expected,
            "{context}: exact argument counts and bytes: {script}"
        );
        false
    }
}

#[test]
fn selected_default_literals_split_but_command_literals_and_quoted_defaults_do_not() {
    let script = r#"IFS=,; unset x; probe args ${x:-a,b}; probe args a,b; probe args ${x:-"a,b"}"#;
    assert_eq!(
        bash(script),
        vec![
            vec![b"a".to_vec(), b"b".to_vec()],
            vec![b"a,b".to_vec()],
            vec![b"a,b".to_vec()]
        ]
    );
    compare(script);
}

#[test]
fn ifs_delimiters_and_read_remainders_match_bash() {
    for script in [
        r#"IFS=,; v=',a,,b,'; probe args $v "$v""#,
        "IFS=' ,'; v=' , a , , b, '; probe args $v",
        r#"IFS=; f() { probe args $@; probe args $*; probe args "$@"; probe args "$*"; }; f 'a b' '' c"#,
        r#"printf '%s\n' 'a,b,' | { IFS=, read x; probe args "$?" "$x"; }"#,
        r#"printf '%s\n' 'a,b,' | { IFS=, read x y; probe args "$?" "$x" "$y"; }"#,
        r#"printf '%s\n' ' a,b ' | { IFS=, read; probe args "$?" "$REPLY"; }"#,
        r#"printf '%s' 'a,b' | { IFS=, read x y; probe args "$?" "$x" "$y"; }"#,
        r#"printf '%s\n' 'a\,b,c' | { IFS=, read x y; probe args "$?" "$x" "$y"; }"#,
        r#"printf '%s\n%s\n' 'a\' 'b,c' | { IFS=, read x y; probe args "$?" "$x" "$y"; }"#,
        r#"IFS=,; f() { local IFS; v='a,b c'; probe args $v; unset IFS; probe args $v; }; f; probe args "$IFS""#,
        r#"IFS=,; v=$(IFS=:; printf 'a:b'); probe args "$IFS" "$v""#,
        r#"probe args "${x:-pre$(printf 'a\nb')post}""#,
    ] {
        compare(script);
    }
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

#[test]
fn generated_ifs_expansions_reads_and_scopes_match_real_bash() {
    const SEED: u64 = 0x4946_535f_4241_5348;
    let mut state = SEED;
    let policies = [
        None,
        Some(""),
        Some(" "),
        Some("\t"),
        Some("\n"),
        Some(" \t\n"),
        Some(","),
        Some(":"),
        Some(" ,"),
        Some(", \t\n"),
        Some(":,"),
        Some(";;"),
        Some("é"),
        Some("é, "),
        Some("💠"),
        Some("\r\u{b}"),
    ];
    let alphabet = [
        "a", "b", " ", "\t", "\n", ",", ":", ";", "é", "💠", "\\", "'", "\r", "\u{b}",
    ];
    let mut cases = 0;
    let mut utf8_refusals = 0;
    eprintln!("IFS corpus seed={SEED:#x}; generated scenarios=4096");
    for (policy, ifs) in policies.iter().enumerate() {
        for form in 0..32 {
            for edge in 0..8 {
                let mut random = String::new();
                for _ in 0..12 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    random.push_str(alphabet[(state as usize) % alphabet.len()]);
                }
                let separator = ifs.unwrap_or(" \t\n");
                let value = match edge {
                    0 => String::new(),
                    1 => separator.repeat(3),
                    2 => format!("{separator}{random}"),
                    3 => format!("{random}{separator}"),
                    4 => format!("a{separator}{separator}b"),
                    5 => format!("{separator}a{separator}b{separator}"),
                    6 => "a\\,b\\ c\\\nd\\".to_owned(),
                    7 => random,
                    _ => unreachable!(),
                };
                let setup = match ifs {
                    Some(ifs) => format!("IFS={};", quote(ifs)),
                    None => "unset IFS;".to_owned(),
                };
                let prefix = format!("{setup}v={};", quote(&value));
                let mut actual_prefix = prefix.clone();
                let mut bash_prefix = prefix;
                if (15..=18).contains(&form) {
                    let elements = [&value, "", "tail ,é"];
                    actual_prefix.push_str(&format!(
                        "arr=$(printf '%s' {});",
                        quote(&serde_json::to_string(&elements).expect("JSON array"))
                    ));
                    bash_prefix.push_str(&format!(
                        "arr=({});",
                        elements
                            .iter()
                            .map(|v| quote(v))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                }
                let body = match form {
                    0 => "probe args $v".to_owned(),
                    1 => "probe args ${v}".to_owned(),
                    2 => "probe args ${missing:-$v}".to_owned(),
                    3 => "probe args $(printf '%s' \"$v\")".to_owned(),
                    4 => "probe args pre${v}post".to_owned(),
                    5 => "probe args ${v}${v}".to_owned(),
                    6 => "probe args ${v}\"\"".to_owned(),
                    7 => "probe args \"\"${v}".to_owned(),
                    8 => "probe args \"$v\"".to_owned(),
                    9 => "probe args \"${missing:-$v}\"".to_owned(),
                    10 => "probe args \"$(printf '%s' \"$v\")\"".to_owned(),
                    11..=14 => {
                        let expansion = ["$@", "$*", "\"$@\"", "\"$*\""][form - 11];
                        format!("f() {{ probe args {expansion}; }}; f \"$v\" '' 'tail ,é'")
                    }
                    15..=18 => {
                        let expansion = ["${arr[@]}", "${arr[*]}", "\"${arr[@]}\"", "\"${arr[*]}\""][form - 15];
                        format!("probe args {expansion}")
                    }
                    19..=24 => {
                        let (read, variables) = match form {
                            19 => ("read a", "\"$a\""),
                            20 => ("read a b", "\"$a\" \"$b\""),
                            21 => ("read a b c", "\"$a\" \"$b\" \"$c\""),
                            22 => ("read a b c d", "\"$a\" \"$b\" \"$c\" \"$d\""),
                            23 => ("read", "\"$REPLY\""),
                            24 => ("read -r a", "\"$a\""),
                            _ => unreachable!(),
                        };
                        let format = if edge % 2 == 0 { "%s\\n" } else { "%s" };
                        format!("printf '{format}' \"$v\" | {{ {read}; probe args \"$?\" {variables}; }}")
                    }
                    25 => "printf '%s\\n' \"$v\" | { IFS=: read a b; probe args \"$?\" \"$a\" \"$b\"; }; probe args \"${IFS-unset}\"".to_owned(),
                    26 => "f() { local IFS=,; probe args $v; unset IFS; probe args $v; }; f; probe args $v \"${IFS-unset}\"".to_owned(),
                    27 => "g() { probe args $v; }; f() { local IFS=:; g; }; f; g".to_owned(),
                    28 => "x=$(IFS=:; printf '%s' \"$v\"); probe args $x \"${IFS-unset}\"".to_owned(),
                    29 => "printf '%s\\n%s\\n' \"$v\" 'tail' | while read -r a b; do probe args \"$a\" \"$b\"; done".to_owned(),
                    30 => "copy=pre${v}post; [[ $copy == \"pre${v}post\" ]] && probe args \"$copy\"; case $v in \"$v\") probe args \"$v\";; esac; captured=$(cat <<EOF\n$v\nEOF\n); probe args \"$captured\"".to_owned(),
                    31 => "probe args ${missing:-a,b:é} ${missing:-\"a,b:é\"} a,b:é".to_owned(),
                    _ => unreachable!(),
                };
                let context =
                    format!("seed={SEED:#x} case={cases} policy={policy} form={form} edge={edge}");
                if compare_pair(
                    &format!("{actual_prefix}{body}"),
                    &format!("{bash_prefix}{body}"),
                    &context,
                ) {
                    utf8_refusals += 1;
                }
                cases += 1;
            }
        }
    }
    eprintln!(
        "IFS corpus: {cases} scenarios; {} exact Bash agreements; {utf8_refusals} explicit UTF-8 refusals",
        cases - utf8_refusals
    );
    assert!(cases - utf8_refusals >= 2000);
    assert_eq!(cases, 4096, "fixed generated corpus size; seed={SEED:#x}");
}

#[test]
fn quoted_arrays_refuse_when_bash_would_emit_invalid_utf8() {
    let script = r#"IFS='é'; arr=$(printf '%s' '["","","tail ,é"]'); probe args "${arr[@]}""#;
    let reference = "IFS='é'; arr=('' '' 'tail ,é'); probe args \"${arr[@]}\"";
    assert!(compare_pair(
        script,
        reference,
        "Bash 5.2 multibyte quoted array"
    ));
}

#[test]
fn protected_defaults_refuse_when_bash_would_emit_invalid_utf8() {
    let script = r#"IFS='é'; probe args ${missing:-"a,b:é"}"#;
    assert!(compare_pair(
        script,
        script,
        "Bash 5.2 multibyte protected default"
    ));
}

#[test]
fn adjacent_empty_ifs_fields_spend_the_step_budget() {
    let script = format!("IFS=,; v='{}'; probe args $v", ",".repeat(100));
    let outcome = run_with(
        &script,
        Limits {
            max_steps: 20,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert!(
        outcome.output.contains("step budget exhausted"),
        "{outcome:?}"
    );
}

#[test]
fn continued_reads_obey_the_shared_value_budget() {
    use std::io::Write as _;

    for (lines, expected) in [(3, ExitCode::SUCCESS), (1000, ExitCode::SYNTAX)] {
        let limits = Limits {
            max_value_bytes: 1024,
            ..Limits::default()
        };
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().expect("script stdin");
        let stdin = dekopon_shell::ChildStdin::adopt(reader).expect("adopt script stdin");
        writer
            .write_all(b"12345678\\\n".repeat(lines).as_slice())
            .expect("continued input lines");
        drop(writer);
        let tree = dekopon_process::TreeContext::new(limits, dekopon_process::CallBudget::new(1));
        let outcome = Interpreter::new(limits).run_child(
            "read x; echo \"$x\"",
            Some(stdin),
            &Fixture::default(),
            &tree,
        );
        assert_eq!(outcome.exit_code, expected, "{outcome:?}");
        if expected == ExitCode::SYNTAX {
            assert!(outcome.output.contains("bytes of values"), "{outcome:?}");
        } else {
            assert_eq!(outcome.output, "12345678".repeat(lines));
        }
    }
}

#[test]
fn review_post_whitespace_leading_byte_collision_refuses_expansion() {
    assert!(compare_pair(
        "IFS=' é'; v='a à'; probe args $v",
        "IFS=' é'; v='a à'; probe args $v",
        "shared UTF-8 leading byte in expansion"
    ));
}

#[test]
fn review_post_whitespace_leading_byte_collision_refuses_read() {
    let script = "IFS=' é'; printf '%s' 'a à' | { read a b; probe args \"$a\" \"$b\"; }";
    assert!(compare_pair(
        script,
        script,
        "shared UTF-8 leading byte in read"
    ));
}

#[test]
fn review_word_splits_after_later_ifs_assignment() {
    for script in [
        "unset IFS; v=a,b; probe args $v${IFS:=,}",
        "unset IFS; v=a,b; probe args ${v}${IFS:=,}",
        "unset IFS; v=a,b; probe args ${missing:-$v}${IFS:=,}",
        "unset IFS; v=a,b; probe args $(printf '%s' \"$v\")${IFS:=,}",
        "unset IFS; v=a,b; probe args \"$v\"${IFS:=,}",
        "unset IFS; probe args a,b${IFS:=,}",
        "unset IFS; probe args ${missing:-\"a,b\"}${IFS:=,}",
        "unset IFS; v=a,b; probe args $v$(IFS=:; printf ':')${IFS:=,}; probe args \"$IFS\"",
    ] {
        compare(script);
    }
}

#[test]
fn review_descendant_unset_removes_the_outer_local_binding() {
    compare("IFS=,; g() { unset IFS; IFS=';'; }; f() { local IFS=:; g; }; f; probe args \"$IFS\"");
}

#[test]
fn review_repeated_bare_local_preserves_the_existing_value() {
    compare("IFS=,; f() { local IFS=:; local IFS; probe args \"$IFS\"; }; f; probe args \"$IFS\"");
}

#[test]
fn review_unset_function_prefix_reveals_the_enclosing_binding() {
    for script in [
        "IFS=,; f() { unset IFS; probe args \"${IFS-unset}\"; }; IFS=: f; probe args \"$IFS\"",
        "IFS=,; f() { local IFS; probe args \"${IFS-unset}\"; }; IFS=: f; probe args \"$IFS\"",
        "IFS=,; f() { IFS=: local IFS; probe args \"${IFS-unset}\"; }; f; probe args \"$IFS\"",
        "IFS=,; f() { local IFS=:; IFS=';' unset IFS; probe args \"${IFS-unset}\"; }; f; probe args \"$IFS\"",
        "IFS=,; f() { unset IFS; IFS=';'; }; IFS=: f; probe args \"$IFS\"",
        "IFS=,; g() { unset IFS; probe args \"${IFS-unset}\"; }; f() { local IFS=';'; IFS=: g; probe args \"$IFS\"; }; f; probe args \"$IFS\"",
    ] {
        compare(script);
    }
}

#[test]
fn deferred_word_fragments_are_charged_before_splitting() {
    for (count, expected) in [(3, ExitCode::SUCCESS), (128, ExitCode::SYNTAX)] {
        let script = format!(
            "unset IFS; v='{}'; : {}${{IFS:= }}",
            " ".repeat(64),
            "${v}".repeat(count)
        );
        let outcome = run_with(
            &script,
            Limits {
                max_value_bytes: 2048,
                ..Limits::default()
            },
        );
        assert_eq!(outcome.exit_code, expected, "{outcome:?}");
        if expected == ExitCode::SYNTAX {
            assert!(outcome.output.contains("bytes of values"), "{outcome:?}");
        }
    }
}

#[test]
fn generated_ifs_review_edges_match_real_bash() {
    let mut cases = 0;
    let mut refused = 0;
    for ifs in [" é", " éĠ", " 💠", " é💠"] {
        for value in ["a à", "a é", "a 💡", "a 💠", " a à b "] {
            for names in 0..=4 {
                let prefix = format!("IFS={};v={};", quote(ifs), quote(value));
                let body = if names == 0 {
                    "probe args $v".to_owned()
                } else {
                    let names = ["a", "b", "c", "d"][..names].join(" ");
                    let args = names
                        .split(' ')
                        .map(|name| format!("\"${name}\""))
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("printf '%s' \"$v\" | {{ read {names}; probe args {args}; }}")
                };
                let script = format!("{prefix}{body}");
                refused += usize::from(compare_pair(
                    &script,
                    &script,
                    &format!("UTF-8 review case={cases}"),
                ));
                cases += 1;
            }
        }
    }
    for initial in [None, Some(""), Some(",")] {
        for final_ifs in [",", ":", "é", " "] {
            for form in 0..14 {
                let setup = initial.map_or_else(
                    || "unset IFS;".to_owned(),
                    |ifs| format!("IFS={};", quote(ifs)),
                );
                let change = format!("${{IFS:={}}}", quote(final_ifs));
                let prefix = format!("{setup}v='a,b:é c';");
                let body = match form {
                    0 => format!("probe args $v{change}"),
                    1 => format!("probe args a,b:é{change}"),
                    2 => format!("probe args \"$v\"{change}"),
                    3 => format!("probe args ${{missing:-$v}}{change}"),
                    4 => format!("probe args ${{missing:-\"a,b:é\"}}{change}"),
                    5 => format!("probe args $(printf '%s' \"$v\"){change}"),
                    6 => format!("probe args $v$(IFS=:; printf ':'){change}; probe args \"$IFS\""),
                    7..=9 => {
                        let expansion = ["$@", "$*", "\"$@\""][form - 7];
                        format!("f() {{ probe args {expansion}{change}; }}; f \"$v\" '' tail")
                    }
                    10..=13 => {
                        let expansion =
                            ["${arr[@]}", "${arr[*]}", "\"${arr[@]}\"", "\"${arr[*]}\""][form - 10];
                        format!("probe args {expansion}{change}")
                    }
                    _ => unreachable!(),
                };
                let (actual_setup, bash_setup) = if form >= 10 {
                    (
                        "arr=$(printf '%s' '[\"a,b:é c\",\"\",\"tail\"]');",
                        "arr=('a,b:é c' '' tail);",
                    )
                } else {
                    ("", "")
                };
                refused += usize::from(compare_pair(
                    &format!("{prefix}{actual_setup}{body}"),
                    &format!("{prefix}{bash_setup}{body}"),
                    &format!("final-IFS review case={cases}"),
                ));
                cases += 1;
            }
        }
    }
    eprintln!(
        "IFS review matrix: {cases} scenarios; {} exact Bash agreements; {refused} explicit UTF-8 refusals",
        cases - refused
    );
    assert_eq!(cases, 268);
}

#[test]
fn quoted_array_assignments_do_not_resplit_multibyte_characters() {
    compare_pair(
        "IFS=é; arr=$(printf '%s' '[\"é\",\"tail\"]'); value=\"${arr[@]}\"; probe args \"$value\"",
        "IFS=é; arr=(é tail); value=\"${arr[@]}\"; probe args \"$value\"",
        "quoted array assignment",
    );
}

#[test]
fn compound_quoted_positionals_refuse_malformed_bash_fields() {
    for form in [r#""x$@""#, r#""$@x""#, r#"x"$@""#, r#""$@"x"#, r#""$@$@""#] {
        let script = format!("IFS=é; f() {{ probe args {form}; }}; f é");
        assert!(compare_pair(
            &script,
            &script,
            "compound quoted positional expansion"
        ));
    }
    compare("IFS=é; f() { probe args \"$@\"; }; f é '' tail");
    compare("IFS=,; f() { probe args \"x$@\"; }; f a,b '' tail");
}

#[test]
fn prefix_bindings_do_not_authorize_top_level_return() {
    let result = run("IFS=: return 7; echo reached:$?");
    assert_eq!(result.exit_code, ExitCode::SUCCESS);
    assert_eq!(
        result.output,
        "dekopon-shell: return: only valid inside a function\nreached:2"
    );
    compare("f() { IFS=: return 7; probe args unreachable; }; f; probe args \"$?\"");
}
