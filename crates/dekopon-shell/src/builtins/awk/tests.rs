use super::{
    AwkError, ChargedRegex, RuntimeError, compile, lex,
    parse::{self, Binary, ExprKind, GawkSyntax, GawkVariable, Refusal, StmtKind, Unary},
};
use crate::{
    builtins::{CommandFailure, FatalError},
    limits::Budget,
};
use dekopon_process::{ExitCode, LimitExceeded, Limits};

fn refused(source: &str) -> Refusal {
    match compile(source, &Budget::start(Limits::default())).unwrap_err() {
        AwkError::Refused(refusal) => refusal,
        other @ (AwkError::Syntax { .. } | AwkError::Runtime(_) | AwkError::Limit(_)) => {
            panic!("expected a refusal for {source:?}, got {other:?}")
        }
    }
}

fn language_refusals() -> Vec<(Refusal, &'static str)> {
    vec![
        (
            Refusal::Getline,
            "awk: `getline` is not supported: awk reads only its one input, one record per rule pass; write a rule per record, or join inputs with the shell before the pipe",
        ),
        (
            Refusal::Process,
            "awk: `system()` is not supported: this shell runs no processes; run the command in the script and pipe its output into awk",
        ),
        (
            Refusal::Redirection,
            "awk: output redirection is not supported: awk writes only to its stdout; redirect the whole command (`awk '…' > buf`, `awk '…' >&2`), or parenthesize a comparison: `print (a > b)`",
        ),
        (
            Refusal::StreamFunction("close".into()),
            "awk: `close()` is not supported: awk opens no files or pipes here",
        ),
        (
            Refusal::StreamFunction("fflush".into()),
            "awk: `fflush()` is not supported: awk opens no files or pipes here",
        ),
        (
            Refusal::Randomness("rand".into()),
            "awk: `rand()` is not supported: the shell has no clock or entropy source",
        ),
        (
            Refusal::Randomness("srand".into()),
            "awk: `srand()` is not supported: the shell has no clock or entropy source",
        ),
        (
            Refusal::UserFunction,
            "awk: user-defined functions are not supported: write the expression inline, or call awk from a shell function",
        ),
        (
            Refusal::UnknownFunction("gensub".into()),
            "awk: `gensub()` is not a function here; the built-in functions are length substr index split sub gsub match sprintf tolower toupper int sqrt exp log sin cos atan2",
        ),
        (
            Refusal::RecordSeparator,
            "awk: `RS` is not supported: records are always lines; split a record yourself with `split($0, parts, sep)`",
        ),
        (
            Refusal::NumberFormat("OFMT".into()),
            "awk: `OFMT` is not supported: numbers print as integers or with %.6g; use printf or sprintf for another format",
        ),
        (
            Refusal::Environment("ENVIRON".into()),
            "awk: `ENVIRON` is not supported: awk sees only its input and its -v assignments; pass values with `-v name=value`",
        ),
        (
            Refusal::GawkVariable(GawkVariable::IgnoreCase),
            "awk: `IGNORECASE` is a gawk variable and is not supported; for case-insensitive matching compare `tolower($0) ~ /re/`",
        ),
        (
            Refusal::GawkVariable(GawkVariable::Fpat),
            "awk: `FPAT` is a gawk variable and is not supported; use FS, split() or substr()",
        ),
        (
            Refusal::MultiSubscript,
            "awk: multi-dimensional subscripts are not supported: join the keys yourself, `a[i \",\" j]`",
        ),
        (
            Refusal::PowerSpelling,
            "awk: `**` is not supported: POSIX awk spells power `^`",
        ),
        (
            Refusal::Nextfile,
            "awk: `nextfile` is not supported: awk has one input; use `exit` to stop reading",
        ),
        (
            Refusal::GawkSyntax(GawkSyntax::Switch),
            "awk: `switch` is a gawk extension and is not supported; use if/else",
        ),
        (
            Refusal::GawkSyntax(GawkSyntax::BeginFile),
            "awk: `BEGINFILE` is a gawk extension and is not supported; awk has one input",
        ),
        (
            Refusal::PrintComparison("==".into()),
            "awk: an unparenthesized comparison in a print list is not POSIX awk; write `print (a == b)`",
        ),
        (
            Refusal::NextOutsideRule,
            "awk: `next` is only allowed in a rule, not in BEGIN or END",
        ),
        (
            Refusal::ScalarArray("x".into()),
            "awk: `x` is used both as an array and as a scalar",
        ),
    ]
}

fn boundary_refusals() -> Vec<(Refusal, &'static str)> {
    vec![
        (
            Refusal::Option("-f".into()),
            "awk: option not yet supported: -f (supported: -F SEP -v NAME=VALUE)",
        ),
        (
            Refusal::OperandAssignment("name=value".into()),
            "awk: operand assignment `name=value` is not supported: use `-v name=value` before the program",
        ),
        (
            Refusal::SecondInput,
            "awk: at most one input operand (a named buffer or `-`); join buffers with `cat a b | awk '…'`",
        ),
        (Refusal::NoProgram, "awk: a program argument is required"),
        (
            Refusal::BadVariable("1=x".into()),
            "awk: -v needs NAME=VALUE with NAME an identifier, got `1=x`",
        ),
        (
            Refusal::BadFieldRegex(
                crate::builtins::text::extended_pattern("awk", &"x".repeat(1025), false)
                    .unwrap_err(),
            ),
            "awk: -E pattern is 1025 bytes; the limit is 1024",
        ),
        (
            Refusal::EmptySeparator,
            "awk: an empty field separator is not supported; take characters with `substr($0, i, 1)`",
        ),
        (
            Refusal::FormatStar,
            "awk: printf `*` width is not supported; build the format with sprintf or concatenation",
        ),
        (
            Refusal::FormatConversion('n'),
            "awk: printf conversion `%n` is not supported; use %c %d %i %o %u %x %X %e %E %f %F %g %G %s or %%",
        ),
        (
            Refusal::ProgramBytes(8193),
            "awk: program is 8193 bytes; the limit is 8192",
        ),
        (
            Refusal::ParseDepth,
            "awk: program nests too deeply (more than 96 parser levels); split it across statements",
        ),
        (
            Refusal::AstHeight,
            "awk: program nests deeper than 32 levels; split it across statements",
        ),
        (
            Refusal::RegexExtension,
            "awk: `(?` is not ERE syntax; awk regexes are POSIX extended regular expressions",
        ),
        (
            Refusal::Regex(
                crate::builtins::text::extended_pattern("awk", &"x".repeat(1025), false)
                    .unwrap_err(),
            ),
            "awk: -E pattern is 1025 bytes; the limit is 1024",
        ),
        (
            Refusal::FormatBound {
                function: "printf".into(),
                dimension: "width".into(),
                value: "4097".into(),
            },
            "awk: `printf` width 4097 is more than 4096; use a width of at most 4096",
        ),
    ]
}

#[test]
fn every_refusal_class_has_its_exact_message() {
    for (refusal, expected) in language_refusals().into_iter().chain(boundary_refusals()) {
        assert_eq!(
            CommandFailure::from(AwkError::Refused(refusal)),
            CommandFailure::Status {
                message: expected.into(),
                status: ExitCode::SYNTAX
            }
        );
    }
    let AwkError::Refused(dynamic) = lex::validate_format("sprintf", "%.4097f").unwrap_err() else {
        panic!("format bound refusal");
    };
    assert_eq!(CommandFailure::from(AwkError::Runtime(RuntimeError { refusal: dynamic, record: 7.0 })), CommandFailure::Status {
        message: "awk: `sprintf` precision 4097 is more than 4096; use a precision of at most 4096 (record 7)".into(), status: ExitCode::SYNTAX,
    });
}

#[test]
fn program_constructs_select_their_refusal_variants() {
    for (source, expected) in [
        ("{getline}", Refusal::Getline),
        ("{x | getline}", Refusal::Getline),
        ("{system(1)}", Refusal::Process),
        ("{print x > y}", Refusal::Redirection),
        ("{printf x >> y}", Refusal::Redirection),
        ("{print x | y}", Refusal::Redirection),
        ("{close(1)}", Refusal::StreamFunction("close".into())),
        ("{rand()}", Refusal::Randomness("rand".into())),
        ("function f() {}", Refusal::UserFunction),
        ("func f(){}", Refusal::UserFunction),
        ("{gensub(1)}", Refusal::UnknownFunction("gensub".into())),
        ("{RS=1}", Refusal::RecordSeparator),
        ("{OFMT}", Refusal::NumberFormat("OFMT".into())),
        ("{ENVIRON[1]}", Refusal::Environment("ENVIRON".into())),
        (
            "{IGNORECASE=1}",
            Refusal::GawkVariable(GawkVariable::IgnoreCase),
        ),
        ("{a[i,j]}", Refusal::MultiSubscript),
        ("{(i,j) in a}", Refusal::MultiSubscript),
        ("{SUBSEP}", Refusal::MultiSubscript),
        ("{x**=2}", Refusal::PowerSpelling),
        ("{nextfile}", Refusal::Nextfile),
        ("BEGINFILE{}", Refusal::GawkSyntax(GawkSyntax::BeginFile)),
        ("@include \"x\"", Refusal::GawkSyntax(GawkSyntax::Include)),
        ("BEGIN{next}", Refusal::NextOutsideRule),
        ("END{next}", Refusal::NextOutsideRule),
        ("{x; x[1]}", Refusal::ScalarArray("x".into())),
        ("{FS=\"\"}", Refusal::EmptySeparator),
        ("{(FS)=(\"\")}", Refusal::EmptySeparator),
        ("{printf \"%*s\", 3, x}", Refusal::FormatStar),
        ("{sprintf(\"%n\",x)}", Refusal::FormatConversion('n')),
        ("/(?i)a/", Refusal::RegexExtension),
    ] {
        assert_eq!(refused(source), expected, "{source}");
    }
    for op in ["<", "<=", "==", "!=", ">="] {
        assert_eq!(
            refused(&format!("{{print a {op} b}}")),
            Refusal::PrintComparison(op.into())
        );
    }
}

#[test]
fn complete_programs_keep_the_frozen_grammar() {
    let programs = [
        "{print $2}",
        "{print $1,$NF}",
        "{$1=$1;print}",
        "NR==2",
        "/err/{n++};END{print n+0}",
        "{s+=$1};END{print s,s/NR}",
        "NF>1&&$2~/^[0-9]+$/{print $1}",
        "{c[$1]++};END{for(k in c)print k,c[k]}",
        "{printf \"%-5s|%5.2f|%03d\\n\",$1,$2,$3}",
        "BEGIN{print length(\"héllo\"),substr(\"hello\",2,3),index(\"hello\",\"ll\"),toupper(\"ab\")}",
        "{n=split($0,p,\",\");print n,p[n]}",
        "{gsub(/o/,\"0\");sub(/^h/,\"H\");print}",
        "match($0,/[0-9]+/){print RSTART,RLENGTH,substr($0,RSTART,RLENGTH)}",
        "/start/,/end/",
        "NR>1{exit};{print};END{print \"done\"}",
        "{if($1>9)print \"big\";else print \"small\"}",
        "BEGIN{x=0.1+0.2;print x,(x==0.3),1e6,1e20,100/3}",
        "BEGIN{while(i<n)print i++}",
        "{$3=\"X\";print;print NF}",
        "END{print NR,$0}",
        "{a[$0]};END{delete a[\"x\"];for(k in a)print k}",
        "BEGIN{print 2^3^2,7%3,-7%3,int(-3.7),\"3\"+\"4\",(\"a\"<\"b\")}",
        "BEGIN{print x+0,\"[\" x \"]\",length(x)}",
        "BEGIN{printf \"%d %s %c %x %e %g\\n\",\"42abc\",3.0,65,255,12345.678,0.0001}",
        "{print NF,$1}",
        "BEGIN{print length(a)};{a[NR]};END{print length(a)}",
        "{print toupper(substr($0,1,1)) substr($0,2)}",
        "BEGIN{print 1 \" \" -1}",
        "BEGIN{for(;;){if(x)break;continue};do\nx-- ;while(x);delete a;print(length(),length)}",
        "BEGIN{for(i=0;i<3;i++)\n{a[i]=sqrt(i)+exp(i)+log(i)+sin(i)+cos(i)+atan2(i,1)}}",
        "# comment\nBEGIN { if(1)\n print(1,2); else\n ; }\n1 &&\n2 ||\n3",
        "{x/=2; x=/=a/; x=x / /a/; x=2\\\n+3}",
    ];
    for program in programs {
        compile(program, &Budget::start(Limits::default()))
            .unwrap_or_else(|error| panic!("{program}: {error:?}"));
    }
}

#[test]
fn concatenation_and_power_keep_their_precedence() {
    let program = parse::program("1 \" \" -1; -2^3^2").unwrap();
    let first = program.rules[0].pattern.as_ref().unwrap();
    let ExprKind::Chain { first, rest } = &first.kind else {
        panic!("concatenation");
    };
    assert_eq!(first.kind, ExprKind::Number(1.0));
    assert_eq!(rest[0].0, Binary::Concat);
    let ExprKind::Chain { first, rest } = &rest[0].1.kind else {
        panic!("subtraction");
    };
    assert_eq!(first.kind, ExprKind::String(" ".into()));
    assert_eq!(rest[0].0, Binary::Sub);
    let ExprKind::Unary(Unary::Minus, power) = &program.rules[1].pattern.as_ref().unwrap().kind
    else {
        panic!("unary");
    };
    let ExprKind::Binary(Binary::Pow, _, right) = &power.kind else {
        panic!("power");
    };
    assert!(matches!(right.kind, ExprKind::Binary(Binary::Pow, _, _)));
}

#[test]
fn source_recursion_and_ast_bounds_are_independent() {
    assert_eq!(refused(&" ".repeat(8193)), Refusal::ProgramBytes(8193));
    assert_eq!(
        refused(&format!("{}1{}", "(".repeat(100), ")".repeat(100))),
        Refusal::ParseDepth
    );
    for (open, close) in [
        ("!", ""),
        ("(", ")"),
        ("$", ""),
        ("length(", ")"),
        ("a[", "]"),
        ("2^", ""),
    ] {
        let accepted = format!("{}1{}", open.repeat(31), close.repeat(31));
        let compiled = compile(&accepted, &Budget::start(Limits::default())).unwrap();
        assert_eq!(
            compiled.program.rules[0].pattern.as_ref().unwrap().height,
            32
        );
        let rejected = format!("{open}{accepted}{close}");
        assert!(matches!(
            refused(&rejected),
            Refusal::AstHeight | Refusal::ParseDepth
        ));
    }
    let flat = std::iter::repeat_n("1", 500).collect::<Vec<_>>().join("+");
    let compiled = compile(&flat, &Budget::start(Limits::default())).unwrap();
    assert_eq!(
        compiled.program.rules[0].pattern.as_ref().unwrap().height,
        2
    );
}

#[test]
fn parser_and_regex_reservations_refund_on_all_return_paths() {
    let budget = Budget::start(Limits {
        max_value_bytes: 2 * 1024 * 1024 - 1,
        ..Limits::default()
    });
    assert!(matches!(
        compile("1", &budget),
        Err(AwkError::Limit(LimitExceeded::ValueBytes { .. }))
    ));
    assert_eq!(budget.value_bytes(), 0);
    let budget = Budget::start(Limits::default());
    assert!(matches!(
        compile("(", &budget),
        Err(AwkError::Syntax { .. })
    ));
    assert_eq!(budget.value_bytes(), 0);
    let compiled = compile("/(a)/", &budget).unwrap();
    assert_eq!(budget.value_bytes(), 256 * 1024);
    assert_eq!(compiled.regexes[0].regex.captures_len(), 1);
    drop(compiled);
    assert_eq!(budget.value_bytes(), 0);
    assert!(matches!(
        compile(&"/a/;".repeat(200), &budget),
        Err(AwkError::Limit(LimitExceeded::ValueBytes { .. }))
    ));
    assert_eq!(budget.value_bytes(), 0);
    assert!(matches!(
        CommandFailure::from(AwkError::Limit(LimitExceeded::ValueBytes { maximum: 1 })),
        CommandFailure::Fatal(FatalError::Limit(LimitExceeded::ValueBytes { maximum: 1 }))
    ));
}

#[test]
fn groups_compile_non_capturing() {
    let budget = Budget::start(Limits::default());
    for source in ["(a)(b)".to_owned(), "((a))+".to_owned(), "(a)".repeat(341)] {
        let regex = ChargedRegex::compile(&source, &budget).unwrap();
        assert_eq!(regex.regex.captures_len(), 1);
        let original = crate::builtins::text::extended_pattern("awk", &source, false).unwrap();
        let input = "ab".repeat(400) + &"a".repeat(400);
        assert_eq!(
            regex.regex.find(&input).map(|m| m.range()),
            original.find(&input).map(|m| m.range())
        );
    }
    for source in ["[(]", "[^(]", "[])(]", "[[:alpha:](]", r"\(", r"[\](]"] {
        assert_eq!(lex::rewrite_regex(source).unwrap(), source);
        let regex = ChargedRegex::compile(source, &budget).unwrap();
        assert_eq!(regex.regex.captures_len(), 1);
        if source == "[^(]" {
            assert!(!regex.regex.is_match("("));
            assert!(regex.regex.is_match("a"));
        } else {
            assert!(regex.regex.is_match("("));
        }
        if source == r"[\](]" {
            assert!(regex.regex.is_match("]"));
        }
    }
    for source in ["(?i)a", "(?:a)", "(?P<n>a)"] {
        assert_eq!(lex::rewrite_regex(source), Err(Refusal::RegexExtension));
    }
    let optional = ChargedRegex::compile(r"\(?[0-9]{3}\)?", &budget).unwrap();
    assert!(optional.regex.is_match("(555)"));
    assert!(optional.regex.is_match("555"));
    assert!(
        ChargedRegex::compile("[(?]", &budget)
            .unwrap()
            .regex
            .is_match("?")
    );
}

#[test]
fn escapes_and_original_regex_diagnostics_survive_translation() {
    assert_eq!(
        lex::escapes(r#"\"\\\/\a\b\f\n\r\t\v\101\q"#),
        "\"\\/\x07\x08\x0c\n\r\t\x0bA\\q"
    );
    let budget = Budget::start(Limits::default());
    let regex = ChargedRegex::compile(r"[\101\b]", &budget).unwrap();
    assert!(regex.regex.is_match("A"));
    assert!(regex.regex.is_match("\x08"));
    assert_eq!(
        refused("/(a/"),
        Refusal::Regex(crate::builtins::text::extended_pattern("awk", "(a", false).unwrap_err())
    );
}

#[test]
fn syntax_errors_and_static_array_checks_are_typed() {
    assert!(matches!(
        compile("BEGIN{\nprint (\n}", &Budget::start(Limits::default())),
        Err(AwkError::Syntax {
            line: 2,
            column: 8,
            ..
        })
    ));
    for source in [
        "{a[1]; a=2}",
        "{split($0,a);print a}",
        "{for(k in a); k[1]}",
        "{delete a; a++}",
    ] {
        assert!(matches!(refused(source), Refusal::ScalarArray(_)));
    }
    let program = compile(
        "BEGIN{print length(a)};{a[1]}",
        &Budget::start(Limits::default()),
    )
    .unwrap();
    assert!(matches!(
        program.program.rules[0].action.as_ref().unwrap().kind,
        StmtKind::Block(_)
    ));
}

#[test]
fn action_boundaries_and_do_while_newlines_keep_following_statements() {
    let program = compile(
        "BEGIN{do x++;while(x<2)\nprint x} {print $1} END{print NR}",
        &Budget::start(Limits::default()),
    )
    .unwrap()
    .program;
    assert_eq!(program.rules.len(), 3);
    let StmtKind::Block(statements) = &program.rules[0].action.as_ref().unwrap().kind else {
        panic!("block");
    };
    assert_eq!(statements.len(), 2);
    assert!(matches!(statements[0].kind, StmtKind::Do(_, _)));
    assert!(matches!(statements[1].kind, StmtKind::Print(_)));
    assert_eq!(refused("BEGIN{} BEGIN{system (1)}"), Refusal::Process);
    assert_eq!(
        refused("{close (1)}"),
        Refusal::StreamFunction("close".into())
    );
    assert!(matches!(
        compile("BEGIN{\n split(x, 1)\n}", &Budget::start(Limits::default())),
        Err(AwkError::Syntax {
            line: 2,
            column: 11,
            ..
        })
    ));
}

#[test]
fn literal_format_bounds_apply_through_the_program_parser() {
    for (function, dimension, format, source) in [
        ("printf", "width", "%4097s", "BEGIN{printf \"%4097s\", x}"),
        (
            "sprintf",
            "precision",
            "%.4097f",
            "BEGIN{x=sprintf(\"%.4097f\", 1)}",
        ),
    ] {
        assert_eq!(
            refused(source),
            Refusal::FormatBound {
                function: function.into(),
                dimension: dimension.into(),
                value: "4097".into()
            }
        );
        assert!(matches!(
            lex::validate_format(function, format),
            Err(AwkError::Refused(Refusal::FormatBound { .. }))
        ));
    }
    assert_eq!(lex::validate_format("printf", "%4096.4096f"), Ok(()));
    assert_eq!(
        lex::validate_format("printf", "%999999999999999999999999s"),
        Err(AwkError::Refused(Refusal::FormatBound {
            function: "printf".into(),
            dimension: "width".into(),
            value: "999999999999999999999999".into()
        }))
    );
}

#[test]
fn malformed_formats_are_syntax_errors_at_the_literal() {
    for format in ["%", "%5", "%.3", "%+"] {
        let program = format!("BEGIN{{\n printf \"{format}\"\n}}");
        assert!(matches!(
            compile(&program, &Budget::start(Limits::default())),
            Err(AwkError::Syntax {
                line: 2,
                column: 9,
                ..
            })
        ));
    }
}

#[test]
fn block_nodes_and_empty_statements_count_toward_height() {
    let source = format!("{};{}", "{".repeat(31), "}".repeat(31));
    let program = compile(&source, &Budget::start(Limits::default())).unwrap();
    assert_eq!(program.program.rules[0].action.as_ref().unwrap().height, 32);
    assert_eq!(refused(&format!("{{{source}}}")), Refusal::AstHeight);
}

#[test]
fn literal_regex_source_and_nesting_are_bounded() {
    assert!(matches!(
        refused(&format!("/{}/", "x".repeat(1025))),
        Refusal::Regex(_)
    ));
    assert!(matches!(
        refused(&format!("/{}x{}/", "(".repeat(17), ")".repeat(17))),
        Refusal::Regex(_)
    ));
}

#[test]
fn parser_thread_admission_is_a_fatal_limit_and_refunds_the_stack() {
    let limits = Limits::default();
    let tree = dekopon_process::TreeContext::new(
        limits,
        dekopon_process::CallBudget::new(1),
        crate::test_threads_up_to(0),
    );
    let budget = Budget::start_tree(limits, tree);
    assert!(matches!(
        compile("1", &budget),
        Err(AwkError::Limit(LimitExceeded::ShellThreads { maximum: 0 }))
    ));
    assert_eq!(budget.value_bytes(), 0);
}

#[test]
fn control_flow_requires_statement_separators_and_array_identifiers() {
    for source in [
        "BEGIN{if(1)print 1 else print 2}",
        "BEGIN{while(1){if(1)break print 2}}",
        "BEGIN{while(1)break print 2}",
        "BEGIN{do print 1 while(0)}",
        "BEGIN{delete if}",
        "BEGIN{delete length}",
        "BEGIN{1 in while}",
    ] {
        assert!(
            matches!(
                compile(source, &Budget::start(Limits::default())),
                Err(AwkError::Syntax { .. })
            ),
            "{source}"
        );
    }
    for source in [
        "BEGIN{if(1){print 1}else{print 2}print 3}",
        "BEGIN{if(1);else;print 3}",
        "BEGIN{while(1)break;print 2}",
        "BEGIN{do{}while(0);do;while(0)}",
    ] {
        compile(source, &Budget::start(Limits::default())).unwrap();
    }
}
