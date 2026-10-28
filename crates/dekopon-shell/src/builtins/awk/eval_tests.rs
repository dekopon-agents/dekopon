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
    let mut budget = Budget::start(Limits {
        max_steps: 100,
        ..Limits::default()
    });
    let options = parse(&["BEGIN{while(1)print 1}END{while(1);}".into()], &budget)
        .unwrap()
        .0;
    assert_eq!(
        run_stream(options, StageReader::Inherited(None), &mut budget, |_| Ok(
            false
        )),
        Ok(ExitCode::SUCCESS)
    );
    assert_eq!(budget.value_bytes(), 0);
}
