use super::*;
use dekopon_process::{LimitExceeded, Limits};

fn run(args: &[&str], input: &[u8], limits: Limits) -> (Result<ExitCode, RunError>, Vec<u8>, u64) {
    let mut budget = Budget::start(limits);
    let arguments = args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let mut output = Vec::new();
    let result =
        parse(&arguments, &budget)
            .map_err(RunError::from)
            .and_then(|(options, operand)| {
                assert!(operand.is_none());
                let reader = PipeReader::from_bytes(
                    input.to_vec(),
                    None,
                    crate::interp::telemetry::stage_read,
                );
                run_stream(
                    options,
                    StageReader::Ready(reader, None),
                    &mut budget,
                    |bytes| {
                        output.extend_from_slice(bytes);
                        Ok(true)
                    },
                )
            });
    (result, output, budget.value_bytes())
}

#[test]
fn rules_fields_and_functions_run_over_a_real_reader() {
    for (program, input, expected) in [
        ("{print $2}", "a b c\nd e f\n", "b\ne\n"),
        ("NR==2", "x\ny\nz\n", "y\n"),
        ("/err/{n++} END{print n+0}", "ok\nerr 1\nerr 2\n", "2\n"),
        (
            "{c[$1]++} END{for(k in c)print k,c[k]}",
            "b\na\nb\n",
            "a 1\nb 2\n",
        ),
        (
            "BEGIN{print length(\"héllo\"),substr(\"hello\",2,3),index(\"hello\",\"ll\"),toupper(\"ab\")}",
            "",
            "5 ell 3 AB\n",
        ),
        ("{n=split($0,p,\",\");print n,p[n]}", "a,b,,c\n", "4 c\n"),
        (
            "{gsub(/o/,\"0\");sub(/^h/,\"H\");print}",
            "hello world\n",
            "Hell0 w0rld\n",
        ),
        (
            "match($0,/[0-9]+/){print RSTART,RLENGTH,substr($0,RSTART,RLENGTH)}",
            "abc123def\n",
            "4 3 123\n",
        ),
        ("/start/,/end/", "a\nstart\nb\nend\nc\n", "start\nb\nend\n"),
        (
            "NR>1{exit}{print}END{print \"done\"}",
            "1\n2\n3\n",
            "1\ndone\n",
        ),
        (
            "{if($1>9)print \"big\";else print \"small\"}",
            "10\n9\nabc\n",
            "big\nsmall\nbig\n",
        ),
        ("{$3=\"X\";print;print NF}", "a\n", "a  X\n3\n"),
        ("END{print NR,$0}", "a\nb\n", "2 b\n"),
        (
            "{a[$0]}END{delete a[\"x\"];for(k in a)print k}",
            "x\ny\n",
            "y\n",
        ),
        (
            "BEGIN{print 2^3^2,7%3,-7%3,int(-3.7),\"3\"+\"4\",(\"a\"<\"b\")}",
            "",
            "512 1 -1 -3 7 1\n",
        ),
        ("BEGIN{print x+0,\"[\" x \"]\",length(x)}", "", "0 [] 0\n"),
        ("{print NF,$1}", "  a   b  ", "2 a\n"),
        (
            "BEGIN{print length(a)}{a[NR]}END{print length(a)}",
            "x\ny\n",
            "0\n2\n",
        ),
        (
            "{print toupper(substr($0,1,1)) substr($0,2)}",
            "élan\n",
            "Élan\n",
        ),
        ("BEGIN{print 1 \" \" -1}", "", "1-1\n"),
    ] {
        let (result, output, retained) = run(&[program], input.as_bytes(), Limits::default());
        assert_eq!(result, Ok(ExitCode::SUCCESS), "{program}");
        assert_eq!(output, expected.as_bytes(), "{program}");
        assert_eq!(retained, 0, "{program}");
    }
}

#[test]
fn argv_initialization_is_ordered_and_fields_do_not_resplit_on_assignment() {
    for (args, input, expected) in [
        (vec!["-F:", "{print $1,$NF}"], "root:x:0:sh\n", "root sh\n"),
        (
            vec!["-F", "\\t", "-vOFS=,", "{$1=$1;print}"],
            "a\tb c\td\n",
            "a,b c,d\n",
        ),
        (
            vec!["-v", "n=3", "BEGIN{while(i<n)print i++}"],
            "",
            "0\n1\n2\n",
        ),
        (
            vec!["-F,", "{$1=\"a,b\";print NF;print $1}"],
            "x,y\n",
            "2\na,b\n",
        ),
        (vec!["-F,", "-vFS=:", "{print $2}"], "a:b\n", "b\n"),
        (vec!["-vFS=:", "-F,", "{print $2}"], "a,b\n", "b\n"),
        (vec!["--", "BEGIN{print 1}"], "", "1\n"),
    ] {
        let (status, output, retained) = run(&args, input.as_bytes(), Limits::default());
        assert_eq!(status, Ok(ExitCode::SUCCESS));
        assert_eq!(output, expected.as_bytes());
        assert_eq!(retained, 0);
    }
}

#[test]
fn begin_only_and_begin_exit_leave_inherited_input_unopened() {
    for program in ["BEGIN{print 1}", "BEGIN{exit}{print}"] {
        let mut budget = Budget::start(Limits::default());
        let options = parse(&[program.into()], &budget).unwrap().0;
        let mut reader =
            PipeReader::from_bytes(b"hi\n".to_vec(), None, crate::interp::telemetry::stage_read);
        assert_eq!(
            run_stream(
                options,
                StageReader::Inherited(Some(&mut reader)),
                &mut budget,
                |_| Ok(true)
            ),
            Ok(ExitCode::SUCCESS)
        );
        assert_eq!(
            reader.read(budget.tree()).unwrap(),
            ReadOutcome::Bytes(b"hi\n".to_vec())
        );
        assert_eq!(budget.value_bytes(), 0);
    }
}

#[test]
fn runtime_errors_keep_prior_output_and_report_the_current_record() {
    for (program, input, kind, record, expected) in [
        (
            "{print 1/$1}",
            b"1\n0\n".as_slice(),
            RuntimeKind::DivisionByZero,
            2.0,
            b"1\n".as_slice(),
        ),
        (
            "{print}",
            b"x\n\xff\n".as_slice(),
            RuntimeKind::Utf8,
            2.0,
            b"x\n".as_slice(),
        ),
        (
            "BEGIN{print $65537}",
            b"".as_slice(),
            RuntimeKind::FieldIndex(65537.0),
            0.0,
            b"".as_slice(),
        ),
        (
            "BEGIN{NF=1e9}",
            b"".as_slice(),
            RuntimeKind::Fields,
            0.0,
            b"".as_slice(),
        ),
        (
            "{FS=$1}",
            b"\n".as_slice(),
            RuntimeKind::EmptySeparator,
            1.0,
            b"".as_slice(),
        ),
    ] {
        let (result, output, retained) = run(&[program], input, Limits::default());
        assert_eq!(
            result,
            Err(RunError::Awk(AwkError::Runtime(RuntimeError {
                kind,
                record
            })))
        );
        assert_eq!(output, expected);
        assert_eq!(retained, 0);
    }
}

#[test]
fn loops_and_retained_results_spend_the_existing_budgets() {
    let (result, _, retained) = run(
        &["BEGIN{while(1);}"],
        b"",
        Limits {
            max_steps: 20,
            ..Limits::default()
        },
    );
    assert!(matches!(
        result,
        Err(RunError::Awk(AwkError::Limit(LimitExceeded::Steps { .. })))
    ));
    assert_eq!(retained, 0);
    let (result, _, retained) = run(&["BEGIN{s=\"x\";while(1)s=s s}"], b"", Limits::default());
    assert!(matches!(
        result,
        Err(RunError::Awk(AwkError::Limit(
            LimitExceeded::ValueBytes { .. }
        )))
    ));
    assert_eq!(retained, 0);
    let (result, output, retained) = run(
        &["{n++}END{print n}"],
        &b"x\n".repeat(50000),
        Limits::default(),
    );
    assert_eq!(result, Ok(ExitCode::SUCCESS));
    assert_eq!(output, b"50000\n");
    assert_eq!(retained, 0);
}

#[test]
fn a_closed_output_stops_begin_before_end() {
    use dekopon_process::pipe::{WriteOutcome, pipe};
    let (mut writer, reader) = pipe((), (), |_, _| {}, |_, _, _| {});
    drop(reader);
    let mut budget = Budget::start(Limits {
        max_steps: 100,
        ..Limits::default()
    });
    let options = parse(&["BEGIN{while(1)print 1}END{while(1);}".into()], &budget)
        .unwrap()
        .0;
    assert_eq!(
        run_stream(
            options,
            StageReader::Inherited(None),
            &mut budget,
            |bytes| {
                Ok(match writer.write(bytes) {
                    WriteOutcome::Accepted => true,
                    WriteOutcome::ReaderGone => false,
                })
            }
        ),
        Ok(ExitCode::SUCCESS)
    );
    assert_eq!(budget.value_bytes(), 0);
}

#[test]
fn output_failures_preserve_fatal_limits_and_the_original_cause() {
    let (result, output, retained) = run(
        &["BEGIN{print 1}"],
        b"",
        Limits {
            max_steps: 1,
            ..Limits::default()
        },
    );
    assert_eq!(
        result,
        Err(RunError::Awk(AwkError::Limit(LimitExceeded::Steps {
            maximum: 1
        })))
    );
    assert_eq!(output, b"1");
    assert_eq!(retained, 0);
    let mut budget = Budget::start(Limits::default());
    let options = parse(&["BEGIN{print 1}".into()], &budget).unwrap().0;
    assert_eq!(
        run_stream(options, StageReader::Inherited(None), &mut budget, |_| {
            Err(CommandFailure::usage("output failure"))
        }),
        Err(RunError::Output(CommandFailure::usage("output failure")))
    );
    assert_eq!(budget.value_bytes(), 0);
}

#[test]
fn argv_triggers_the_frozen_refusals_before_input_exists() {
    for (arguments, expected) in [
        (vec![], Refusal::NoProgram),
        (vec!["-f", "file"], Refusal::Option("-f".into())),
        (vec!["-x", "{}"], Refusal::Option("-x".into())),
        (vec!["{}", "n=1"], Refusal::OperandAssignment("n=1".into())),
        (vec!["{}", "a", "b"], Refusal::SecondInput),
        (vec!["-vn", "{}"], Refusal::BadVariable("n".into())),
        (vec!["-v", "1=x", "{}"], Refusal::BadVariable("1=x".into())),
        (vec!["-vRS=x", "{}"], Refusal::RecordSeparator),
        (vec!["-v", "", "{}"], Refusal::BadVariable("".into())),
        (vec!["-F", "", "{}"], Refusal::EmptySeparator),
        (vec!["-va=1", "{a[1]}"], Refusal::ScalarArray("a".into())),
    ] {
        let budget = Budget::start(Limits::default());
        let args = arguments.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            parse(&args, &budget).unwrap_err(),
            AwkError::Refused(expected)
        );
        assert_eq!(budget.value_bytes(), 0);
    }
    let budget = Budget::start(Limits::default());
    assert!(matches!(
        parse(&["-F[?".into(), "{}".into()], &budget),
        Err(AwkError::Refused(Refusal::BadFieldRegex(_)))
    ));
    assert_eq!(budget.value_bytes(), 0);
    for operand in ["buf", "-"] {
        let (_, selected) = parse(&["{}".into(), operand.into()], &budget).unwrap();
        assert_eq!(selected.as_deref(), Some(operand));
    }
}

#[test]
fn loops_ranges_and_array_snapshots_keep_their_control_flow() {
    for (program, input, expected) in [
        (
            "BEGIN{for(i=0;i<5;i++){if(i==1)continue;if(i==4)break;s+=i}do{s++}while(s<10);while(1){s--;break}print s}",
            "",
            "9\n",
        ),
        (
            "{if(NR==2)next;print}END{print NR}",
            "a\nb\nc\n",
            "a\nc\n3\n",
        ),
        (
            "{if(NR==1)FNR=9;print NR,FNR}END{print NR}",
            "a\nb\n",
            "9 9\n10 10\n10\n",
        ),
        ("/x/,/x/", "a\nx\nb\nx\nc\n", "x\nx\n"),
        (
            "BEGIN{a[\"c\"];a[\"a\"];a[\"b\"];for(k in a){print k;delete a[\"b\"];a[\"d\"]=1}print length(a);delete a;print length(a)}",
            "",
            "a\nc\n3\n0\n",
        ),
        (
            "BEGIN{print(\"x\" in a),length(a);print a[\"x\"];print(\"x\" in a),length(a)}",
            "",
            "0 0\n\n1 1\n",
        ),
        (
            "BEGIN{x=0;print(0&&x++),(1||x++),x,(1?5:1/0)}",
            "",
            "0 1 0 5\n",
        ),
        (
            "BEGIN{i=1;a[1]=10;a[2]=20;a[i++]+=3;print i,a[1],a[2]}",
            "",
            "2 13 20\n",
        ),
    ] {
        let (status, output, retained) = run(&[program], input.as_bytes(), Limits::default());
        assert_eq!(status, Ok(ExitCode::SUCCESS), "{program}");
        assert_eq!(output, expected.as_bytes(), "{program}");
        assert_eq!(retained, 0);
    }
}

#[test]
fn strnums_and_numeric_prefixes_preserve_lexical_types() {
    for (args, expected) in [
        (
            vec!["-vn=01", "BEGIN{print(n==1),(n==\"1\"),(n==\"01\"),(n<2)}"],
            "1 0 1 1\n",
        ),
        (
            vec!["BEGIN{print(x==0),(x==\"\"),(x==\"0\"),(\"10\"<2)}"],
            "1 1 0 1\n",
        ),
        (
            vec!["BEGIN{print \" -3.5e+2tail\"+0,\"1e+\"+0,\"0x1A\"+0,\"inf\"+0,\"nan\"+0}"],
            "-350 1 0 0 0\n",
        ),
        (
            vec!["BEGIN{print sqrt(4),exp(0),log(1),sin(0),cos(0),atan2(0,1)}"],
            "2 1 0 0 1 0\n",
        ),
        (
            vec![
                "BEGIN{print substr(\"abcd\",0,2),substr(\"abcd\",2.4,1.5),substr(\"abcd\",-2,3),tolower(\"ÉLAN\")}",
            ],
            "a bc  élan\n",
        ),
    ] {
        let (status, output, retained) = run(&args, b"", Limits::default());
        assert_eq!(status, Ok(ExitCode::SUCCESS));
        assert_eq!(output, expected.as_bytes());
        assert_eq!(retained, 0);
    }
}

#[test]
fn substitutions_and_regexes_keep_the_frozen_divergences() {
    for (program, expected) in [
        (
            r#"BEGIN{print split("abc",a,(/./)),"["a[4]"]";print match("abc",(/b/));s="abc";print sub((/b/),"X",s),s;print ("abc"~(/b/))}"#,
            "4 []\n2\n1 aXc\n1\n",
        ),
        (r#"BEGIN{s="abc";print gsub(/x*/,"-",s),s}"#, "4 -a-b-c-\n"),
        (r#"BEGIN{s="aaab";print gsub(/a*/,"X",s),s}"#, "2 XbX\n"),
        (r#"BEGIN{s="ab";print gsub(/^|$/,"X",s),s}"#, "2 XabX\n"),
        (r#"BEGIN{s="ab";print gsub(/.*/,"X",s),s}"#, "1 X\n"),
        (r#"BEGIN{s="ab";sub(/a|ab/,"X",s);print s}"#, "Xb\n"),
        (r#"BEGIN{s="a";gsub(/a/,"\\\\&",s);print s}"#, "\\a\n"),
        (r#"BEGIN{s="a";gsub(/a/,"\\&",s);print s}"#, "&\n"),
        (
            r#"BEGIN{print match("a\nb",/a.b/),match("a\nb",/a[^x]b/)}"#,
            "0 1\n",
        ),
        (
            r#"BEGIN{print split("abc",a,/x*/),a[1];print split("abc",a,/./),"["a[4]"]"}"#,
            "1 abc\n4 []\n",
        ),
        (
            r#"BEGIN{print match("é123",/[0-9]+/),RSTART,RLENGTH;print match("x",/z/),RSTART,RLENGTH}"#,
            "2 2 3\n0 0 -1\n",
        ),
    ] {
        let (status, output, retained) = run(&[program], b"", Limits::default());
        assert_eq!(status, Ok(ExitCode::SUCCESS), "{program}");
        assert_eq!(output, expected.as_bytes(), "{program}");
        assert_eq!(retained, 0);
    }
}

#[test]
fn dynamic_regex_errors_have_runtime_variants() {
    for source in ["(?i)a", "(?:a)", "(?P<n>a)"] {
        let (result, _, retained) = run(
            &["{$0 ~ $0}"],
            format!("{source}\n").as_bytes(),
            Limits::default(),
        );
        assert_eq!(
            result,
            Err(RunError::Awk(AwkError::Runtime(RuntimeError {
                kind: RuntimeKind::Refused(Refusal::RegexExtension),
                record: 1.0
            })))
        );
        assert_eq!(retained, 0);
    }
    let (result, _, retained) = run(&["{$0 ~ $0}"], b"[\n", Limits::default());
    assert!(matches!(
        result,
        Err(RunError::Awk(AwkError::Runtime(RuntimeError {
            kind: RuntimeKind::Refused(Refusal::Regex(_)),
            record: 1.0
        })))
    ));
    assert_eq!(retained, 0);
    for program in ["BEGIN{split(\"x\",a,\"\")}", "BEGIN{split(\"x\",a,//)}"] {
        let (result, _, _) = run(&[program], b"", Limits::default());
        assert!(matches!(
            result,
            Err(RunError::Awk(AwkError::Runtime(RuntimeError {
                kind: RuntimeKind::EmptySeparator,
                record: 0.0
            })))
        ));
    }
}

#[test]
fn exit_status_is_modular_and_end_preserves_an_earlier_exit() {
    for (program, status, expected) in [
        ("BEGIN{exit 3}", 3, ""),
        ("BEGIN{exit 258}END{print 7;exit}END{print 8}", 2, "7\n"),
        ("BEGIN{exit -1}END{exit 4}", 4, ""),
        ("BEGIN{exit -1}", 255, ""),
    ] {
        let (result, output, retained) = run(&[program], b"unread\n", Limits::default());
        assert_eq!(result, Ok(ExitCode::from_script_exit(status)));
        assert_eq!(output, expected.as_bytes());
        assert_eq!(retained, 0);
    }
}

#[test]
fn multiplicative_results_fail_before_building_and_refund_retention() {
    let s = format!("s={}", "a".repeat(1024 * 1024));
    let t = format!("t={}", "x".repeat(1024 * 1024));
    let (result, output, retained) = run(
        &["-v", &s, "-v", &t, "BEGIN{print 1;gsub(/x*/,t,s)}"],
        b"",
        Limits::default(),
    );
    assert!(matches!(
        result,
        Err(RunError::Awk(AwkError::Limit(
            LimitExceeded::ValueBytes { .. }
        )))
    ));
    assert_eq!(output, b"1\n");
    assert_eq!(retained, 0);
    let ofs = format!("OFS={}", "x".repeat(8 * 1024 * 1024));
    let (result, _, retained) = run(&["-v", &ofs, "BEGIN{NF=65536}"], b"", Limits::default());
    assert!(matches!(
        result,
        Err(RunError::Awk(AwkError::Limit(
            LimitExceeded::ValueBytes { .. }
        )))
    ));
    assert_eq!(retained, 0);
}

#[test]
fn field_caps_apply_to_records_split_and_assignments() {
    let input = format!("{}\n", ",".repeat(65536));
    let (result, _, retained) = run(&["-F,", "{print}"], input.as_bytes(), Limits::default());
    assert!(matches!(
        result,
        Err(RunError::Awk(AwkError::Runtime(RuntimeError {
            kind: RuntimeKind::Fields,
            record: 1.0
        })))
    ));
    assert_eq!(retained, 0);
    let source = format!("s={}", ",".repeat(65536));
    let (result, _, retained) = run(
        &["-v", &source, "BEGIN{split(s,a,\",\")}"],
        b"",
        Limits::default(),
    );
    assert!(matches!(
        result,
        Err(RunError::Awk(AwkError::Runtime(RuntimeError {
            kind: RuntimeKind::Fields,
            record: 0.0
        })))
    ));
    assert_eq!(retained, 0);
    let (result, output, retained) = run(
        &["BEGIN{NF=65536;print NF;NF=0;print NF}"],
        b"",
        Limits::default(),
    );
    assert_eq!(result, Ok(ExitCode::SUCCESS));
    assert_eq!(output, b"65536\n0\n");
    assert_eq!(retained, 0);
}

#[test]
fn the_dynamic_cache_holds_sixteen_charged_entries_and_evicts() {
    let limits = Limits {
        max_value_bytes: 4 * 1024 * 1024 + 4096,
        ..Limits::default()
    };
    let mut budget = Budget::start(limits);
    let tree = budget.tree().clone();
    let options = parse(&["BEGIN{for(i=0;i<40;i++)print match(\"x17\",\"x\" i);for(i=0;i<40;i++)print match(\"x17\",\"x\" i)}".into()], &budget).unwrap().0;
    let mut peak = 0;
    assert_eq!(
        run_stream(options, StageReader::Inherited(None), &mut budget, |_| {
            peak = peak.max(tree.value_bytes());
            Ok(true)
        }),
        Ok(ExitCode::SUCCESS)
    );
    assert!(peak >= 16 * 256 * 1024);
    assert!(peak <= limits.max_value_bytes);
    assert_eq!(budget.value_bytes(), 0);
}

#[test]
fn print_streams_shared_large_values_without_joining_them() {
    let mut budget = Budget::start(Limits::default());
    let tree = budget.tree().clone();
    let options = parse(
        &["BEGIN{s=\"x\";for(i=0;i<24;i++)s=s s;print s,s,s}".into()],
        &budget,
    )
    .unwrap()
    .0;
    let (mut bytes, mut peak) = (0, 0);
    assert_eq!(
        run_stream(
            options,
            StageReader::Inherited(None),
            &mut budget,
            |chunk| {
                bytes += chunk.len();
                peak = peak.max(tree.value_bytes());
                Ok(true)
            }
        ),
        Ok(ExitCode::SUCCESS)
    );
    assert_eq!(bytes, 3 * 16 * 1024 * 1024 + 3);
    assert!(peak < 17 * 1024 * 1024);
    assert_eq!(budget.value_bytes(), 0);
}

#[test]
fn repeated_finds_and_straight_line_scans_observe_the_deadline() {
    let setup = "BEGIN{s=\"x\";for(i=0;i<16;i++)s=s s;r=\"[^y]*y|[^z]*z|x\";";
    for body in [
        "gsub(r,\"\",s)".to_owned(),
        "split(s,a,r)".to_owned(),
        "match(s,r);".repeat(400),
    ] {
        let program = format!("{setup}{body}}}");
        let (result, _, retained) = run(
            &[&program],
            b"",
            Limits {
                timeout: std::time::Duration::from_millis(100),
                ..Limits::default()
            },
        );
        assert!(matches!(
            result,
            Err(RunError::Awk(AwkError::Limit(
                LimitExceeded::Deadline { .. }
            )))
        ));
        assert_eq!(retained, 0);
    }
}

#[test]
fn the_deepest_evaluator_trees_run_on_a_two_mib_stack() {
    std::thread::Builder::new()
        .stack_size(ShellThreads::STACK_BYTES)
        .spawn(|| {
            for (open, close) in [
                ("!", ""),
                ("(", ")"),
                ("$", ""),
                ("length(", ")"),
                ("a[", "]"),
            ] {
                let program = format!("{}1{}", open.repeat(31), close.repeat(31));
                let (result, _, retained) = run(&[&program], b"a\n", Limits::default());
                assert_eq!(result, Ok(ExitCode::SUCCESS), "{program}");
                assert_eq!(retained, 0);
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn record_boundaries_utf8_and_field_updates_survive_chunking() {
    let input = format!("{}\nlast", "é".repeat(4097));
    let (result, output, retained) =
        run(&["{print length($0)}"], input.as_bytes(), Limits::default());
    assert_eq!(result, Ok(ExitCode::SUCCESS));
    assert_eq!(output, b"4097\n4\n");
    assert_eq!(retained, 0);
    let (result, output, retained) = run(
        &["{FS=\",\";print NF;$0=\"a,b\";print NF;OFS=\":\";NF=3;print;NF=1;print}"],
        b"a,b\nc,d\n",
        Limits::default(),
    );
    assert_eq!(result, Ok(ExitCode::SUCCESS));
    assert_eq!(output, b"1\n2\na:b:\na\n2\n2\na:b:\na\n");
    assert_eq!(retained, 0);
}
