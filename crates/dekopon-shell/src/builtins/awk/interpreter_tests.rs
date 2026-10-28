use crate::{Interpreter, ScriptOutcome, builtins::test_support::NoCapabilities};
use dekopon_process::{ExitCode, Limits};

fn run(script: &str, limits: Limits) -> ScriptOutcome {
    Interpreter::new(limits, crate::test_threads()).run(script, &NoCapabilities)
}

fn run_input(script: &str, input: Vec<u8>, limits: Limits) -> ScriptOutcome {
    let reader = dekopon_process::pipe::PipeReader::from_bytes(
        input,
        None,
        crate::interp::telemetry::stage_read,
    );
    run_reader(script, reader, limits)
}

fn run_reader(
    script: &str,
    reader: dekopon_process::pipe::PipeReader<crate::interp::telemetry::StageObservation>,
    limits: Limits,
) -> ScriptOutcome {
    let tree = dekopon_process::TreeContext::new(
        limits,
        dekopon_process::CallBudget::new(1),
        crate::test_threads(),
    );
    let outcome = Interpreter::new(limits, crate::test_threads()).run_child(
        script,
        Some(crate::ChildStdin(reader)),
        &NoCapabilities,
        &tree,
    );
    assert_eq!(tree.value_bytes(), 0);
    outcome
}

#[test]
fn dynamic_formats_fail_at_the_current_record_with_their_refusal_variant() {
    use super::parse::Refusal;
    for (format, refusal) in [
        ("%*s", Refusal::FormatStar),
        ("%n", Refusal::FormatConversion('n')),
        (
            "%4097s",
            Refusal::FormatBound {
                function: "printf".into(),
                dimension: "width".into(),
                value: "4097".into(),
            },
        ),
        (
            "%.4097f",
            Refusal::FormatBound {
                function: "printf".into(),
                dimension: "precision".into(),
                value: "4097".into(),
            },
        ),
    ] {
        let outcome = run_input(
            &format!(
                "awk -v fmt={} 'NR==1{{print \"ok\"}} NR==2{{printf fmt,1}}'",
                quote(format)
            ),
            b"1\n2\n".to_vec(),
            Limits::default(),
        );
        let expected =
            crate::builtins::CommandFailure::from(super::AwkError::Runtime(super::RuntimeError {
                kind: super::RuntimeKind::Refused(refusal),
                record: 2.0,
            }));
        let crate::builtins::CommandFailure::Status { message, status } = expected else {
            panic!("usage error")
        };
        assert_eq!(outcome.exit_code, status);
        assert_eq!(outcome.output, format!("ok\n{message}"));
    }
    let outcome = run_input(
        "awk -v fmt=% '{printf fmt}'",
        b"x\n".to_vec(),
        Limits::default(),
    );
    let error =
        crate::builtins::CommandFailure::from(super::AwkError::Runtime(super::RuntimeError {
            kind: super::RuntimeKind::FormatSyntax {
                line: 1,
                column: 2,
                expected: "a conversion after `%`".into(),
            },
            record: 1.0,
        }));
    let crate::builtins::CommandFailure::Status { message, status } = error else {
        panic!("usage error")
    };
    assert_eq!(outcome.exit_code, status);
    assert_eq!(outcome.output, message);
}

#[test]
fn regex_literal_charges_refuse_before_the_inherited_reader_opens() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static READ: AtomicUsize = AtomicUsize::new(0);
    let reader =
        dekopon_process::pipe::PipeReader::from_bytes(b"hi\n".to_vec(), None, |bytes, _| {
            READ.fetch_add(bytes, Ordering::Relaxed);
        });
    let program = "/(.{1,90}){1,8}z/;".repeat(200);
    let outcome = run_reader(
        &format!("awk {}", quote(&program)),
        reader,
        Limits::default(),
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert_eq!(READ.load(Ordering::Relaxed), 0);
    assert_eq!(
        outcome.output,
        crate::interp::fatal_message(&crate::builtins::FatalError::Limit(
            dekopon_process::LimitExceeded::ValueBytes {
                maximum: Limits::default().max_value_bytes
            }
        ))
    );
}

#[test]
fn repeated_regex_searches_reread_the_interpreters_deadline() {
    let setup = "BEGIN{s=\"x\";for(i=0;i<16;i++)s=s s;r=\"[^y]*y|[^z]*z|x\";";
    for body in [
        "gsub(r,\"\",s)".to_owned(),
        "split(s,a,r)".to_owned(),
        "match(s,r);".repeat(400),
    ] {
        let outcome = run(
            &format!("awk {}", quote(&format!("{setup}{body}}}"))),
            Limits {
                timeout: std::time::Duration::from_millis(100),
                ..Limits::default()
            },
        );
        assert_eq!(outcome.exit_code.get(), 124);
        assert_eq!(
            outcome.output,
            crate::interp::fatal_message(&crate::builtins::FatalError::Limit(
                dekopon_process::LimitExceeded::Deadline { timeout_ms: 100 }
            ))
        );
    }
    let outcome = run_input(
        "awk -F '[^y]*y|[^z]*z|x' '{print NF}'",
        vec![b'x'; 65536],
        Limits {
            timeout: std::time::Duration::from_millis(100),
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code.get(), 124);
    assert_eq!(
        outcome.output,
        crate::interp::fatal_message(&crate::builtins::FatalError::Limit(
            dekopon_process::LimitExceeded::Deadline { timeout_ms: 100 }
        ))
    );
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn stdout(script: &str, expected: &str) {
    let outcome = run(
        &format!("{script}; result=$?; printf '.'; exit \"$result\""),
        Limits::default(),
    );
    assert_eq!(
        outcome.exit_code,
        ExitCode::SUCCESS,
        "{script}: {}",
        outcome.output
    );
    assert_eq!(outcome.output, format!("{expected}."), "{script}");
}

#[test]
fn the_twenty_nine_frozen_programs_run_through_interpreter() {
    for (argv, input, expected) in [
        (r#"'{print $2}'"#, "a b c\nd e f\n", "b\ne\n"),
        (
            r#"-F: '{print $1, $NF}'"#,
            "root:x:0:0:/root:/bin/sh\n",
            "root /bin/sh\n",
        ),
        (
            r#"-F'\t' -v OFS=, '{$1=$1; print}'"#,
            "a\tb c\td\n",
            "a,b c,d\n",
        ),
        (r#"'NR==2'"#, "x\ny\nz\n", "y\n"),
        (
            r#"'/err/ {n++} END {print n+0}'"#,
            "ok\nerr 1\nerr 2\n",
            "2\n",
        ),
        (
            r#"'{s += $1} END {print s, s/NR}'"#,
            "1\n2\n4\n",
            "7 2.33333\n",
        ),
        (
            r#"'NF > 1 && $2 ~ /^[0-9]+$/ {print $1}'"#,
            "a 1\nb x\nc 22\nd\n",
            "a\nc\n",
        ),
        (
            r#"'{c[$1]++} END {for (k in c) print k, c[k]}'"#,
            "b\na\nb\n",
            "a 1\nb 2\n",
        ),
        (
            r#"'{printf "%-5s|%5.2f|%03d\n", $1, $2, $3}'"#,
            "ab 3.14159 7\n",
            "ab   | 3.14|007\n",
        ),
        (
            r#"'BEGIN {print length("héllo"), substr("hello", 2, 3), index("hello", "ll"), toupper("ab")}'"#,
            "",
            "5 ell 3 AB\n",
        ),
        (
            r#"'{n = split($0, p, ","); print n, p[n]}'"#,
            "a,b,,c\n",
            "4 c\n",
        ),
        (
            r#"'{gsub(/o/, "0"); sub(/^h/, "H"); print}'"#,
            "hello world\n",
            "Hell0 w0rld\n",
        ),
        (
            r#"'match($0, /[0-9]+/) {print RSTART, RLENGTH, substr($0, RSTART, RLENGTH)}'"#,
            "abc123def\n",
            "4 3 123\n",
        ),
        (
            r#"'/start/,/end/'"#,
            "a\nstart\nb\nend\nc\n",
            "start\nb\nend\n",
        ),
        (
            r#"'NR > 1 {exit} {print} END {print "done"}'"#,
            "1\n2\n3\n",
            "1\ndone\n",
        ),
        (
            r#"'{ if ($1 > 9) print "big"; else print "small" }'"#,
            "10\n9\nabc\n",
            "big\nsmall\nbig\n",
        ),
        (
            r#"'BEGIN {x = 0.1 + 0.2; print x, (x == 0.3), 1e6, 1e20, 100/3}'"#,
            "",
            "0.3 0 1000000 1e+20 33.3333\n",
        ),
        (
            r#"-v n=3 'BEGIN {while (i < n) print i++}'"#,
            "",
            "0\n1\n2\n",
        ),
        (r#"'{$3 = "X"; print; print NF}'"#, "a\n", "a  X\n3\n"),
        (r#"'END {print NR, $0}'"#, "a\nb\n", "2 b\n"),
        (
            r#"'{a[$0]} END {delete a["x"]; for (k in a) print k}'"#,
            "x\ny\n",
            "y\n",
        ),
        (
            r#"'BEGIN {print 2^3^2, 7%3, -7%3, int(-3.7), "3" + "4", ("a" < "b")}'"#,
            "",
            "512 1 -1 -3 7 1\n",
        ),
        (
            r#"'BEGIN {print x+0, "[" x "]", length(x)}'"#,
            "",
            "0 [] 0\n",
        ),
        (
            r#"'BEGIN {printf "%d %s %c %x %e %g\n", "42abc", 3.0, 65, 255, 12345.678, 0.0001}'"#,
            "",
            "42 3 A ff 1.234568e+04 0.0001\n",
        ),
        (r#"'{print NF, $1}'"#, "  a   b  ", "2 a\n"),
        (r#"'{print $1}' buf"#, "p q\n", "p\n"),
        (
            r#"'BEGIN {print length(a)} {a[NR]} END {print length(a)}'"#,
            "x\ny\n",
            "0\n2\n",
        ),
        (
            r#"'{print toupper(substr($0, 1, 1)) substr($0, 2)}'"#,
            "élan\n",
            "Élan\n",
        ),
        (r#"'BEGIN {print 1 " " -1}'"#, "", "1-1\n"),
    ] {
        let script = if argv.ends_with(" buf") {
            format!("printf '%s' {} > buf; awk {argv}", quote(input))
        } else {
            format!("printf '%s' {} | awk {argv}", quote(input))
        };
        stdout(&script, expected);
    }
}

#[test]
fn awk_diagnostics_preserve_formatted_fragments() {
    for sink in [" | cat", ""] {
        let outcome = run(
            &format!("awk 'BEGIN{{printf \"[%03d]\",1}}' >&2{sink}"),
            Limits::default(),
        );
        assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
        assert_eq!(outcome.output, "[001]", "{sink}");
    }
}

#[test]
fn awk_diagnostics_preserve_utf8_across_emitter_chunks() {
    let text = format!("{}é", "a".repeat(4095));
    for sink in [" | cat", ""] {
        let outcome = run(
            &format!(
                "awk -v text={} 'BEGIN{{printf \"%s\",text}}' >&2{sink}",
                quote(&text)
            ),
            Limits::default(),
        );
        assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
        assert_eq!(outcome.output, text, "{sink}");
    }
}

#[test]
fn awk_diagnostics_split_only_at_actual_newlines() {
    for sink in [" | cat", ""] {
        let outcome = run(
            &format!(
                "{{ awk 'BEGIN{{printf \"a\";printf \"\\n\\nb\";printf \"c\"}}' >&2; awk 'BEGIN{{printf \"d\"}}' >&2; }}{sink}"
            ),
            Limits::default(),
        );
        assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
        assert_eq!(outcome.output, "a\n\nbcd", "{sink}");
    }
}

#[test]
fn awk_diagnostics_keep_partial_lines_when_execution_fails() {
    let failure =
        crate::builtins::CommandFailure::from(super::AwkError::Runtime(super::RuntimeError {
            kind: super::RuntimeKind::DivisionByZero,
            record: 0.0,
        }));
    let crate::builtins::CommandFailure::Status { message, status } = failure else {
        panic!("usage error")
    };
    for sink in [" | cat", ""] {
        let outcome = run(
            &format!("set -o pipefail; awk 'BEGIN{{printf \"[%03d]\",1;print 1/0}}' >&2{sink}"),
            Limits::default(),
        );
        assert_eq!(outcome.exit_code, status);
        assert_eq!(outcome.output, format!("[001]{message}"), "{sink}");
    }
    let outcome = run(
        "awk 'BEGIN{printf \"[%03d]\",1;while(1);}' >&2 | cat",
        Limits {
            max_steps: 100,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    assert_eq!(
        outcome.output,
        format!(
            "[001]\n{}",
            crate::interp::fatal_message(&crate::builtins::FatalError::Limit(
                dekopon_process::LimitExceeded::Steps { maximum: 100 }
            ))
        )
    );
}

#[test]
fn awk_diagnostic_fragments_spend_the_existing_byte_and_line_limits() {
    let outcome = run(
        "{ awk 'BEGIN{s=\"é\";for(i=0;i<16;i++)s=s s;printf \"%s\",s}' >&2; } 2>diag; awk '{print length($0)}' diag",
        Limits {
            max_output_bytes: 65,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{}", outcome.output);
    assert_eq!(outcome.output, "32\n40");
    let outcome = run(
        "{ awk 'BEGIN{for(i=0;i<5;i++)printf \"%02d\\n\",i}' >&2; } 2>diag; awk 'END{print NR,$0}' diag",
        Limits {
            max_output_lines: 2,
            ..Limits::default()
        },
    );
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
    assert_eq!(outcome.output, "3 ... redirected diagnostics truncated ...");
}

#[test]
fn printf_and_sprintf_keep_flags_precision_and_character_widths() {
    for (program, expected) in [
        (
            r#"BEGIN{printf "[%+08.3d][%#.0o][%#.4o][%#08x][%#8.4x][%-08d]",12,0,9,10,10,12}"#,
            "[    +012][0][0011][0x00000a][  0x000a][12      ]",
        ),
        (
            r#"BEGIN{printf "%i %u %o %x %X",-12,-1,-1,-1,255}"#,
            "-12 18446744073709551615 1777777777777777777777 ffffffffffffffff FF",
        ),
        (
            r#"BEGIN{printf "%d %d",1e20,-9223372036854775808}"#,
            "100000000000000000000 -9223372036854775808",
        ),
        (
            r#"BEGIN{printf "[%5.3s][%-5.2s][%3c][%c]","élan","héllo",128512,"éa"}"#,
            "[  éla][hé   ][  😀][é]",
        ),
        (
            r#"BEGIN{printf "%g %.3g %.3g %.0g %#.3g %#.3g",999999.5,999.5,0.00009999,12,12,1e5}"#,
            "1e+06 1e+03 0.0001 1e+01 12.0 1.00e+05",
        ),
        (
            r#"BEGIN{printf "%+.0e %#.0E %#.0f %.2F %G",12,12,12,12.345,1e-8}"#,
            "+1e+01 1.E+01 12. 12.35 1E-08",
        ),
        (
            r#"BEGIN{printf "[%s][%d][%f][%%]";print;print sprintf("%.2f:%s",3.14159,"x",i++),i}"#,
            "[][0][0.000000][%]\n3.14:x 1\n",
        ),
        (
            r#"BEGIN{print 1e309,-1e309;printf "%+8f %08E",1e309,-1e309}"#,
            "inf -inf\n    +inf     -INF",
        ),
        (
            r#"BEGIN{printf "%.0d|%.0u|%.0x|%#.0x|%#.0o|%.0s",0,0,0,0,0,"abc"}"#,
            "||||0|",
        ),
    ] {
        stdout(&format!("awk {}", quote(program)), expected);
    }
}

#[test]
fn maximum_format_precision_fits_the_bounded_numeric_renderer() {
    stdout(
        "awk 'BEGIN{print length(sprintf(\"%.4096f\",1e308)),length(sprintf(\"%.4096e\",1e308)),length(sprintf(\"%#.4096g\",1e308)),length(sprintf(\"%#.4096g\",1e-323))}'",
        "4406 4103 4097 4102\n",
    );
    stdout(
        "awk 'BEGIN{printf \"%4096s\",\"é\"}'",
        &format!("{}é", " ".repeat(4095)),
    );
}

#[test]
fn numeric_characters_truncate_before_selecting_the_unicode_scalar() {
    stdout(
        "awk 'BEGIN{printf \"[%c][%c][%c]\",-0.5,65.9,1114111.5}'",
        "[\0][A][\u{10ffff}]",
    );
}

#[test]
fn lazy_begin_and_refusals_preserve_the_groups_input() {
    for (program, expected) in [
        ("BEGIN{print 1}", "1\nhi\n"),
        ("BEGIN{exit}{print}", "hi\n"),
    ] {
        stdout(
            &format!("printf 'hi\\n' | {{ awk {}; cat; }}", quote(program)),
            expected,
        );
    }
    stdout(
        "printf 'hi\\n' | { awk '{getline}' 2>/dev/null; status=$?; cat; echo $status; }",
        "hi\n2\n",
    );
    stdout("awk --help", "awk: -F SEP -v NAME=VALUE\n");
}

#[test]
fn runtime_failure_keeps_output_in_every_destination() {
    stdout(
        "printf '1\\n0\\n' | awk '{print 1/$1}' > buf 2>/dev/null; status=$?; cat buf; echo $status",
        "1\n2\n",
    );
    stdout(
        "printf '1\\n0\\n' | awk '{print 1/$1}' 2>/dev/null; echo $?",
        "1\n2\n",
    );
    stdout(
        "x=$(printf '1\\n0\\n' | awk '{print 1/$1}' 2>/dev/null); status=$?; echo $x; echo $status",
        "1\n2\n",
    );
    stdout(
        "awk 'BEGIN{printf \"hello\";print 1/0}' 2>err >buf; status=$?; cat buf; echo $status",
        "hello2\n",
    );
}

fn nested_shell(program: &str) -> String {
    let mut script = format!("f0() {{ awk {}; }}; ", quote(program));
    for n in 1..31 {
        script.push_str(&format!("f{n}() {{ f{}; }}; ", n - 1));
    }
    script.push_str("printf '0\\n' | f30");
    script
}

#[test]
fn the_deepest_awk_trees_run_inside_thirty_one_shell_calls_on_two_mib() {
    let [permit] = crate::test_threads().reserve().unwrap();
    std::thread::scope(|scope| {
        permit
            .spawn(
                scope,
                dekopon_process::ThreadName::new("awk-deep-shell"),
                || {
                    for (open, close) in [
                        ("!", ""),
                        ("(", ")"),
                        ("$", ""),
                        ("a[", "]"),
                        ("length(", ")"),
                        ("sprintf(\"%.4096f\",", ")"),
                        ("2^", ""),
                        ("x=", ""),
                    ] {
                        let program = format!("{}0{}", open.repeat(31), close.repeat(31));
                        let outcome = run(&nested_shell(&program), Limits::default());
                        assert_eq!(
                            outcome.exit_code,
                            ExitCode::SUCCESS,
                            "{program}: {}",
                            outcome.output
                        );
                        let program = format!("{}0{}", open.repeat(32), close.repeat(32));
                        assert!(matches!(
                            super::compile(
                                &program,
                                &crate::limits::Budget::start(Limits::default())
                            ),
                            Err(super::AwkError::Refused(
                                super::parse::Refusal::AstHeight
                                    | super::parse::Refusal::ParseDepth
                            ))
                        ));
                        let outcome = run(&nested_shell(&program), Limits::default());
                        assert_eq!(
                            outcome.exit_code,
                            ExitCode::SYNTAX,
                            "{program}: {}",
                            outcome.output
                        );
                    }
                    for (open, close, leaf) in [("if(1)", "", ";"), ("if(0);else ", "", ";")] {
                        let program =
                            format!("BEGIN{{{}{leaf}{}}}", open.repeat(30), close.repeat(30));
                        assert_eq!(
                            run(&nested_shell(&program), Limits::default()).exit_code,
                            ExitCode::SUCCESS,
                            "{program}"
                        );
                        let program =
                            format!("BEGIN{{{}{leaf}{}}}", open.repeat(31), close.repeat(31));
                        assert_eq!(
                            run(&nested_shell(&program), Limits::default()).exit_code,
                            ExitCode::SYNTAX,
                            "{program}"
                        );
                    }
                    let pairs = [("(", ")"), ("a[", "]"), ("length(", ")"), ("$", "")];
                    let mut expression = "0".to_owned();
                    for n in 0..31 {
                        let (open, close) = pairs[n % pairs.len()];
                        expression = format!("{open}{expression}{close}");
                    }
                    assert_eq!(
                        run(&nested_shell(&expression), Limits::default()).exit_code,
                        ExitCode::SUCCESS
                    );
                },
            )
            .unwrap()
            .join()
            .unwrap();
    });
}

#[test]
fn flat_chains_and_many_records_keep_their_iteration_bounds() {
    let addition = std::iter::repeat_n("1", 500).collect::<Vec<_>>().join("+");
    stdout(&format!("awk 'BEGIN{{print {addition}}}'"), "500\n");
    let fields = (1..=40)
        .map(|n| format!("${n}"))
        .collect::<Vec<_>>()
        .join(" \",\" ");
    stdout(
        &format!("printf 'x\\n' | awk '{{print {fields}}}'"),
        &format!("x{}\n", ",".repeat(39)),
    );
    let outcome = run_input(
        "awk '{n++}END{print n}'",
        b"x\n".repeat(50000),
        Limits::default(),
    );
    assert_eq!(outcome.exit_code, ExitCode::SUCCESS);
    assert_eq!(outcome.output, "50000");
}

#[test]
fn runtime_data_failures_and_exit_statuses_survive_registration() {
    for (program, kind) in [
        (
            "BEGIN{print $65537}",
            super::RuntimeKind::FieldIndex(65537.0),
        ),
        ("BEGIN{NF=1e9}", super::RuntimeKind::Fields),
        ("BEGIN{s=\"\";FS=s}", super::RuntimeKind::EmptySeparator),
        ("BEGIN{print 1/0}", super::RuntimeKind::DivisionByZero),
    ] {
        let expected =
            crate::builtins::CommandFailure::from(super::AwkError::Runtime(super::RuntimeError {
                kind,
                record: 0.0,
            }));
        let crate::builtins::CommandFailure::Status { message, status } = expected else {
            panic!("usage error")
        };
        let outcome = run(&format!("awk {}", quote(program)), Limits::default());
        assert_eq!(outcome.exit_code, status);
        assert_eq!(outcome.output, message);
    }
    for (program, status) in [
        ("BEGIN{exit 3}", 3),
        ("BEGIN{exit 258}", 2),
        ("BEGIN{exit -1}END{exit}", 255),
    ] {
        assert_eq!(
            run(&format!("awk {}", quote(program)), Limits::default())
                .exit_code
                .get(),
            status
        );
    }
    let program = "BEGIN{\nprint )\n}";
    assert!(matches!(
        super::compile(program, &crate::limits::Budget::start(Limits::default())),
        Err(super::AwkError::Syntax {
            line: 2,
            column: 7,
            ..
        })
    ));
    assert_eq!(
        run(&format!("awk {}", quote(program)), Limits::default()).exit_code,
        ExitCode::SYNTAX
    );
}

#[test]
fn invalid_utf8_input_fails_after_preserving_earlier_output() {
    use std::io::Write;
    let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
    let input = crate::ChildStdin::adopt(reader).unwrap();
    writer.write_all(b"x\n\xff\n").unwrap();
    drop(writer);
    let limits = Limits::default();
    let tree = dekopon_process::TreeContext::new(
        limits,
        dekopon_process::CallBudget::new(1),
        crate::test_threads(),
    );
    let outcome = Interpreter::new(limits, crate::test_threads()).run_child(
        "awk '{print}'",
        Some(input),
        &NoCapabilities,
        &tree,
    );
    assert_eq!(outcome.exit_code, ExitCode::SYNTAX);
    let expected =
        crate::builtins::CommandFailure::from(super::AwkError::Runtime(super::RuntimeError {
            kind: super::RuntimeKind::Utf8,
            record: 2.0,
        }));
    let crate::builtins::CommandFailure::Status { message, .. } = expected else {
        panic!("usage error")
    };
    assert_eq!(outcome.output, format!("x\n{message}"));
}

#[test]
fn retained_results_and_loop_steps_are_fatal_through_interpreter() {
    use dekopon_process::LimitExceeded;
    let limits = Limits::default();
    for program in [
        "BEGIN{s=\"x\";while(1)s=s s}",
        "BEGIN{s=\"a\";for(i=0;i<20;i++)s=s s;t=s;gsub(/x*/,t,s)}",
        "BEGIN{s=\"x\";for(i=0;i<23;i++)s=s s;OFS=s;NF=65536}",
        "BEGIN{s=\"x\";for(i=0;i<24;i++)s=s s;t=sprintf(\"%s%s%s\",s,s,s)}",
    ] {
        let outcome = run(&format!("awk {}", quote(program)), limits);
        assert_eq!(outcome.exit_code, ExitCode::SYNTAX, "{program}");
        assert_eq!(
            outcome.output,
            crate::interp::fatal_message(&crate::builtins::FatalError::Limit(
                LimitExceeded::ValueBytes {
                    maximum: limits.max_value_bytes
                }
            )),
            "{program}"
        );
    }
    let outcome = run(
        "awk 'BEGIN{while(1);}'",
        Limits {
            max_steps: 100,
            ..limits
        },
    );
    assert_eq!(
        outcome.output,
        crate::interp::fatal_message(&crate::builtins::FatalError::Limit(LimitExceeded::Steps {
            maximum: 100
        }))
    );
}

#[test]
fn large_print_and_printf_stream_to_the_existing_output_ceiling() {
    for output in ["print s,s,s", "printf \"%s%s%s\",s,s,s"] {
        let outcome = run(
            &format!("awk 'BEGIN{{s=\"x\";for(i=0;i<24;i++)s=s s;{output}}}'"),
            Limits::default(),
        );
        assert_eq!(outcome.exit_code, ExitCode::SUCCESS, "{}", outcome.output);
        assert!(outcome.truncated);
    }
}
