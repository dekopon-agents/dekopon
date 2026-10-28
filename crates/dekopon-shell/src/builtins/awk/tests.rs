use super::{CompiledProgram, compile, compile_regex, compile_separator, failure, lex, parse};
use crate::{builtins::CommandFailure, limits::Budget};
use dekopon_process::{ExitCode, LimitExceeded, Limits};
use parse::{AwkError, Binary, ExprKind, Pattern, Refusal, RuntimeError, StmtKind};

fn parsed(source: &str) -> CompiledProgram {
    compile(source, &Budget::start(Limits::default())).expect(source)
}
fn refused(source: &str) -> Refusal {
    match compile(source, &Budget::start(Limits::default())).expect_err(source) {
        AwkError::Refused(refusal) => refusal,
        error @ (AwkError::Syntax { .. } | AwkError::Runtime { .. } | AwkError::Limit(_)) => {
            panic!("{source}: {error:?}")
        }
    }
}

enum Case<'a> {
    Program(&'a str),
    Refusal(fn() -> Refusal),
    Separator(&'a str),
    LongRegex(usize),
}
const REFUSAL_CASES: &[(Case<'static>, &str)] = &[
    (
        Case::Program("{getline}"),
        "awk: `getline` is not supported: awk reads only its one input, one record per rule pass; write a rule per record, or join inputs with the shell before the pipe",
    ),
    (
        Case::Program("{system(\"x\")}"),
        "awk: `system()` is not supported: this shell runs no processes; run the command in the script and pipe its output into awk",
    ),
    (
        Case::Program("{print 1 > x}"),
        "awk: output redirection is not supported: awk writes only to its stdout; redirect the whole command (`awk '…' > buf`, `awk '…' >&2`), or parenthesize a comparison: `print (a > b)`",
    ),
    (
        Case::Program("{close(1)}"),
        "awk: `close()` is not supported: awk opens no files or pipes here",
    ),
    (
        Case::Program("{rand()}"),
        "awk: `rand()` is not supported: the shell has no clock or entropy source",
    ),
    (
        Case::Program("function f(){}"),
        "awk: user-defined functions are not supported: write the expression inline, or call awk from a shell function",
    ),
    (
        Case::Program("{gensub() }"),
        "awk: `gensub()` is not a function here; the built-in functions are length substr index split sub gsub match sprintf tolower toupper int sqrt exp log sin cos atan2",
    ),
    (
        Case::Program("RS"),
        "awk: `RS` is not supported: records are always lines; split a record yourself with `split($0, parts, sep)`",
    ),
    (
        Case::Program("OFMT"),
        "awk: `OFMT` is not supported: numbers print as integers or with %.6g; use printf or sprintf for another format",
    ),
    (
        Case::Program("ENVIRON"),
        "awk: `ENVIRON` is not supported: awk sees only its input and its -v assignments; pass values with `-v name=value`",
    ),
    (
        Case::Program("IGNORECASE"),
        "awk: `IGNORECASE` is a gawk variable and is not supported; for case-insensitive matching compare `tolower($0) ~ /re/`",
    ),
    (
        Case::Program("FPAT"),
        "awk: `FPAT` is a gawk variable and is not supported; use FS, split() or substr()",
    ),
    (
        Case::Program("{a[i,j]}"),
        "awk: multi-dimensional subscripts are not supported: join the keys yourself, `a[i \",\" j]`",
    ),
    (
        Case::Program("{2**3}"),
        "awk: `**` is not supported: POSIX awk spells power `^`",
    ),
    (
        Case::Program("{nextfile}"),
        "awk: `nextfile` is not supported: awk has one input; use `exit` to stop reading",
    ),
    (
        Case::Program("{switch(x){}}"),
        "awk: `switch` is a gawk extension and is not supported; use if/else",
    ),
    (
        Case::Program("BEGINFILE{}"),
        "awk: `BEGINFILE` is a gawk extension and is not supported; awk has one input",
    ),
    (
        Case::Program("{print a == b}"),
        "awk: an unparenthesized comparison in a print list is not POSIX awk; write `print (a == b)`",
    ),
    (
        Case::Program("BEGIN{next}"),
        "awk: `next` is only allowed in a rule, not in BEGIN or END",
    ),
    (
        Case::Program("{x=1; x[1]}"),
        "awk: `x` is used both as an array and as a scalar",
    ),
    (
        Case::Refusal(|| Refusal::Option("-f".into())),
        "awk: option not yet supported: -f (supported: -F SEP -v NAME=VALUE)",
    ),
    (
        Case::Refusal(|| Refusal::OperandAssignment("name=value".into())),
        "awk: operand assignment `name=value` is not supported: use `-v name=value` before the program",
    ),
    (
        Case::Refusal(|| Refusal::SecondInput),
        "awk: at most one input operand (a named buffer or `-`); join buffers with `cat a b | awk '…'`",
    ),
    (
        Case::Refusal(|| Refusal::NoProgram),
        "awk: a program argument is required",
    ),
    (
        Case::Refusal(|| Refusal::BadAssignment("1=x".into())),
        "awk: -v needs NAME=VALUE with NAME an identifier, got `1=x`",
    ),
    (
        Case::Separator("[a"),
        "awk: -E pattern \"[a\" did not compile: non-empty character class has no closing bracket",
    ),
    (
        Case::Program("{FS=\"\"}"),
        "awk: an empty field separator is not supported; take characters with `substr($0, i, 1)`",
    ),
    (
        Case::Program("{printf \"%*s\",x}"),
        "awk: printf `*` width is not supported; build the format with sprintf or concatenation",
    ),
    (
        Case::Program("{printf \"%n\",x}"),
        "awk: printf conversion `%n` is not supported; use %c %d %i %o %u %x %X %e %E %f %F %g %G %s or %%",
    ),
    (
        Case::Refusal(|| Refusal::ProgramBytes(8193)),
        "awk: program is 8193 bytes; the limit is 8192",
    ),
    (
        Case::Refusal(|| Refusal::ParseDepth),
        "awk: program nests too deeply (more than 96 parser levels); split it across statements",
    ),
    (
        Case::Refusal(|| Refusal::AstHeight),
        "awk: program nests deeper than 32 levels; split it across statements",
    ),
    (
        Case::Refusal(|| Refusal::PrintfWidth(4097)),
        "awk: printf width is 4097; the limit is 4096",
    ),
    (
        Case::Refusal(|| Refusal::PrintfPrecision(4097)),
        "awk: printf precision is 4097; the limit is 4096",
    ),
    (
        Case::Program("/(?i)a/"),
        "awk: `(?` is not ERE syntax; awk regexes are POSIX extended regular expressions",
    ),
    (
        Case::Program("/(a/"),
        "awk: -E pattern \"(a\" did not compile: found open group without closing ')'",
    ),
    (
        Case::LongRegex(1025),
        "awk: -E pattern is 1025 bytes; the limit is 1024",
    ),
];

#[test]
fn every_refusal_class_has_its_exact_message() {
    for (case, expected) in REFUSAL_CASES {
        let error = match case {
            Case::Program(source) => {
                compile(source, &Budget::start(Limits::default())).expect_err(source)
            }
            Case::Refusal(refusal) => AwkError::Refused(refusal()),
            Case::LongRegex(size) => compile(
                &format!("/{}/", "a".repeat(*size)),
                &Budget::start(Limits::default()),
            )
            .expect_err("oversized regex"),
            Case::Separator(source) => {
                compile_separator(source, &Budget::start(Limits::default())).expect_err(source)
            }
        };
        assert_eq!(
            failure(error),
            CommandFailure::Status {
                message: (*expected).into(),
                status: ExitCode::SYNTAX
            }
        );
    }
}

#[test]
fn all_forbidden_names_and_spellings_are_recognized() {
    for source in ["{getline x}", "{x|getline}", "{getline < x}"] {
        assert_eq!(refused(source), Refusal::Getline);
    }
    for name in ["close", "fflush"] {
        assert_eq!(
            refused(&format!("{{{name}()}}")),
            Refusal::StreamFunction(name.into())
        );
    }
    for name in ["rand", "srand"] {
        assert_eq!(
            refused(&format!("{{{name}()}}")),
            Refusal::Randomness(name.into())
        );
    }
    for name in ["OFMT", "CONVFMT"] {
        assert_eq!(refused(name), Refusal::NumberFormat(name.into()));
    }
    for name in ["ENVIRON", "ARGV", "ARGC", "FILENAME"] {
        assert_eq!(refused(name), Refusal::Environment(name.into()));
    }
    for name in [
        "IGNORECASE",
        "FPAT",
        "FIELDWIDTHS",
        "PROCINFO",
        "RT",
        "BINMODE",
        "LINT",
        "TEXTDOMAIN",
        "ERRNO",
    ] {
        assert_eq!(
            refused(name),
            Refusal::GawkVariable(parse::GawkVariable::lookup(name).expect("gawk name"))
        );
    }
    for name in ["BEGINFILE", "ENDFILE", "@include", "@load", "switch"] {
        assert_eq!(
            refused(name),
            Refusal::GawkSyntax(parse::GawkSyntax::lookup(name).expect("gawk syntax"))
        );
    }
    for source in ["{a[i,j]}", "(i,j) in a", "{delete a[i,j]}", "SUBSEP"] {
        assert_eq!(refused(source), Refusal::MultiSubscript);
    }
    for source in ["{x**2}", "{x**=2}"] {
        assert_eq!(refused(source), Refusal::PowerSpelling);
    }
    assert_eq!(refused("func f(){}"), Refusal::UserFunction);
    assert_eq!(refused("END{if(1) next}"), Refusal::NextOutsideRule);
    for source in ["{print a >> b}", "{printf a | b}", "{printf a > b}"] {
        assert_eq!(refused(source), Refusal::OutputRedirection);
    }
    for op in ["<", "<=", "==", "!=", ">="] {
        assert_eq!(
            refused(&format!("{{print a {op} b}}")),
            Refusal::PrintComparison(op.into())
        );
    }
}

#[test]
fn the_program_shape_preserves_source_order_and_ranges() {
    let p = parsed(
        "BEGIN{print 1} BEGIN{print 2} /a/,/b/; NR==2 {next} {print}; END{print 3} END{print 4}",
    );
    assert_eq!(
        (
            p.program.begin.len(),
            p.program.rules.len(),
            p.program.end.len()
        ),
        (2, 3, 2)
    );
    assert!(matches!(p.program.rules[0].pattern, Pattern::Range(_, _)));
    assert!(matches!(p.program.rules[2].pattern, Pattern::Always));
    for source in [
        "",
        "# comment",
        "BEGIN{print 1}",
        "END{print NR}",
        "/=/",
        "/[/]/",
        "/[[:alpha:]/]/",
        "0 ? 1 : x=2",
        "\"x\" ++b",
        "\"x\" --b",
        "{x = 8 / 2; x /= 2}",
    ] {
        parsed(source);
    }
}

#[test]
fn the_kept_statement_and_function_grammar_compiles() {
    let sources = [
        "BEGIN {if (1) print 1; else if (2) print 2; else print 3}",
        "{while (i<4) {if(i++) continue; break}; do {i--} while(i); for(;;) break; for(i=0;i<3;i++) print i; for(k in a) delete a[k]; delete a; next; exit 258}",
        "{print; print(a,b); print (a) + b; print (a>b); printf(\"%d\",a>b)}",
        "{a[1]=2; print length(a), length(), length, substr($0,2), substr($0,2,3), index($0,\"x\"); n=split($0,a,/x/); split($0,a); sub(/x/,\"y\"); gsub(/x/,\"y\",$1); match($0,/x/); print sprintf(\"%s\",x),tolower(x),toupper(x),int(x),sqrt(x),exp(x),log(x),sin(x),cos(x),atan2(x,y)}",
        "{x=1; x+=2; x-=2; x*=2; x/=2; x%=2; x^=2; ++x; --x; x++; x--; x=(1?2:3); x=(a in members); x=!x; x=+x; x=-x; x=(a~ /b/); x=(a !~ /b/); x=(a<=b); x=(a>=b); x=(a!=b); x=(a==b); x=(a>b); x=(a<b); print .5,1.,1e-3,2E+4}",
        "{if (1)\n print 1; else\n print 2; while (0)\n; do\n; while(0); for(;;)\n break; print 1,\n 2; x=1 &&\n 2 ||\n 3}",
        "# header\n{print \\\n 1} # tail\n",
        "{print \"getline RS (? **\"; print system,rand; x=1; print x (x+1)}",
    ];
    for source in sources {
        parsed(source);
    }
}

#[test]
fn concatenation_and_power_have_the_frozen_precedence() {
    let p = parsed("1 \" \" -1; -2^3^2; 1+2*3; a=b=c");
    match &p.program.rules[0].pattern {
        Pattern::Expr(expr) => match &expr.kind {
            ExprKind::Chain(first, rest) => {
                assert_eq!(first.kind, ExprKind::Number(1.0));
                assert_eq!(rest[0].0, Binary::Concat);
                assert!(matches!(rest[0].1.kind, ExprKind::Chain(_, _)));
            }
            other @ (ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Regex(_)
            | ExprKind::Variable(_)
            | ExprKind::Array(_, _)
            | ExprKind::Field(_)
            | ExprKind::Group(_)
            | ExprKind::Call(_, _)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment(_, _)
            | ExprKind::Binary(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)) => panic!("{other:?}"),
        },
        other @ (Pattern::Always | Pattern::Range(_, _)) => panic!("{other:?}"),
    }
    assert!(matches!(
        p.program.rules[1].pattern,
        Pattern::Expr(parse::Expr {
            kind: ExprKind::Unary(_, _),
            ..
        })
    ));
    assert!(matches!(
        p.program.rules[3].pattern,
        Pattern::Expr(parse::Expr {
            kind: ExprKind::Binary(Binary::Assign, _, _),
            ..
        })
    ));
}

#[test]
fn scalar_array_checks_defer_ambiguous_length_uses() {
    parsed("BEGIN{print length(a)} {a[NR]} END{print length(a)}");
    parsed("{print length(x); x=1}");
    for source in [
        "{x=1; x[1]}",
        "{x[1]; x=1}",
        "{split($0,x); print x}",
        "{for(x in x) print 1}",
        "{delete x; print x}",
    ] {
        assert_eq!(refused(source), Refusal::ScalarArray("x".into()));
    }
    assert_eq!(refused("{FS[1]}"), Refusal::ScalarArray("FS".into()));
}

#[test]
fn syntax_errors_preserve_line_and_column() {
    let error = compile("{\n print (1\n}", &Budget::start(Limits::default()))
        .expect_err("unclosed grouping");
    assert!(matches!(
        error,
        AwkError::Syntax {
            line: 2,
            column: 10,
            ..
        }
    ));
    for source in [
        "BEGIN",
        "END",
        "{print 1",
        "{a=}",
        "{a<b<c}",
        "{a~b~c}",
        "{substr(1)}",
        "{split(1,2)}",
        "{sub(/x/,1,2)}",
        "{(1)++}",
        "{break}",
    ] {
        assert!(
            matches!(
                compile(source, &Budget::start(Limits::default())),
                Err(AwkError::Syntax { .. })
            ),
            "{source}"
        );
    }
}

#[test]
fn parse_bounds_cover_source_depth_height_and_flat_chains() {
    assert_eq!(refused(&" ".repeat(8193)), Refusal::ProgramBytes(8193));
    assert_eq!(
        refused(&format!("{}1", "!".repeat(96))),
        Refusal::ParseDepth
    );
    for (open, close) in [
        ("!", ""),
        ("(", ")"),
        ("$", ""),
        ("a[", "]"),
        ("length(", ")"),
    ] {
        let source = format!("{}1{}", open.repeat(31), close.repeat(31));
        let p = parsed(&source);
        match &p.program.rules[0].pattern {
            Pattern::Expr(expr) => assert_eq!(expr.height, 32),
            pattern @ (Pattern::Always | Pattern::Range(_, _)) => panic!("{pattern:?}"),
        }
        assert_eq!(
            refused(&format!("{}1{}", open.repeat(32), close.repeat(32))),
            if open == "a[" || open == "length(" {
                Refusal::ParseDepth
            } else {
                Refusal::AstHeight
            }
        );
    }
    let chain = std::iter::repeat_n("1", 500).collect::<Vec<_>>().join("+");
    let p = parsed(&chain);
    match &p.program.rules[0].pattern {
        Pattern::Expr(expr) => assert_eq!(expr.height, 2),
        pattern @ (Pattern::Always | Pattern::Range(_, _)) => panic!("{pattern:?}"),
    }
    parsed(&format!(
        "{{print {}}}",
        (1..=40)
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(" \",\" ")
    ));
    assert_eq!(
        refused(&format!("{}1", "2^".repeat(33))),
        Refusal::AstHeight
    );
    assert_eq!(
        refused(&format!(
            "{{{}print 1{}}}",
            "if(1){".repeat(33),
            "}".repeat(33)
        )),
        Refusal::AstHeight
    );
}

#[test]
fn literal_formats_are_checked_without_a_formatter() {
    for source in ["{printf \"%4097s\"}", "{sprintf(\"%4097s\")}"] {
        assert_eq!(refused(source), Refusal::PrintfWidth(4097));
    }
    assert_eq!(
        refused("{printf \"%.4097f\"}"),
        Refusal::PrintfPrecision(4097)
    );
    assert_eq!(refused("{sprintf(\"%.*s\",1)}"), Refusal::PrintfStar);
    parsed(
        "{printf f; sprintf(f); printf \"%-+ #04096.4096F %% %c %d %i %o %u %x %X %e %E %f %g %G %s\"}",
    );
    for source in ["{FS=\"\"}", "{FS=(\"\")}"] {
        assert_eq!(refused(source), Refusal::EmptySeparator);
    }
}

#[test]
fn groups_compile_non_capturing() {
    let budget = Budget::start(Limits::default());
    for source in ["(a)(b)".into(), "((a))+".into(), "(a)".repeat(341)] {
        let compiled = compile_regex(&source, &budget).expect("groups compile");
        assert_eq!(compiled.regex.captures_len(), 1);
        let oracle = regex_bites::Regex::new(&source).expect("original compiles");
        let input = "ab".repeat(341);
        assert_eq!(
            compiled.regex.find(&input).map(|m| (m.start(), m.end())),
            oracle.find(&input).map(|m| (m.start(), m.end()))
        );
        assert_eq!(budget.value_bytes(), 256 * 1024);
    }
    for source in ["[(]", "[^(]", "[])(]", "[[:alpha:](]", r"\(", r"[\](]"] {
        assert_eq!(lex::regex_source(source).expect("ERE"), source);
        let compiled = compile_regex(source, &budget).expect("literal bracket contents");
        assert_eq!(compiled.regex.captures_len(), 1);
        assert_eq!(compiled.regex.is_match("("), source != "[^(]");
        if source == r"[\](]" {
            assert!(compiled.regex.is_match("]"));
            assert!(!compiled.regex.is_match("?"));
            assert!(!compiled.regex.is_match(":"));
        }
    }
}

#[test]
fn regex_extensions_are_refused_and_awk_escapes_are_translated() {
    for source in ["/(?i)a/", "/(?:a)/", "/(?P<n>a)/"] {
        assert_eq!(refused(source), Refusal::RegexExtension);
    }
    let p = parsed(r"/\(?[0-9]{3}\)?/; /[(?]/; /\101\b/; /[\050]/");
    assert!(p.regexes[0].regex.is_match("(555)"));
    assert!(p.regexes[0].regex.is_match("555"));
    assert!(p.regexes[1].regex.is_match("?"));
    assert!(p.regexes[2].regex.is_match("A\u{0008}"));
    assert!(p.regexes[3].regex.is_match("("));
    assert_eq!(
        lex::decode_escapes(r#"\"\\\/\a\b\f\n\r\t\v\101\12\1\q"#).expect("escapes"),
        "\"\\/\u{0007}\u{0008}\u{000c}\n\r\t\u{000b}A\n\u{0001}\\q"
    );
    assert!(
        compile_separator(":", &Budget::start(Limits::default()))
            .expect("literal separator")
            .is_none()
    );
    assert!(
        compile_separator("[,:]", &Budget::start(Limits::default()))
            .expect("regex separator")
            .is_some()
    );
}

#[test]
fn regex_and_parser_charges_are_reserved_and_refunded() {
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
        refused(&format!("/{}/", "a".repeat(1025))),
        Refusal::Regex(_)
    ));
    let result = compile(&"/(.{1,90}){1,8}z/;".repeat(200), &budget);
    assert!(matches!(
        result,
        Err(AwkError::Limit(LimitExceeded::ValueBytes { .. }))
    ));
    assert_eq!(budget.value_bytes(), 0);
    let p = compile("/(a)(b)/", &budget).expect("literal regex");
    assert_eq!(budget.value_bytes(), 256 * 1024 + "/(a)(b)/".len() as u64);
    drop(p);
    assert_eq!(budget.value_bytes(), 0);
}

#[test]
fn error_mapping_keeps_limits_fatal_and_runtime_errors_at_exit_two() {
    let limit = LimitExceeded::Steps { maximum: 1 };
    assert_eq!(
        failure(AwkError::Limit(limit)),
        CommandFailure::Fatal(crate::builtins::FatalError::Limit(limit))
    );
    for error in [
        RuntimeError::DivisionByZero,
        RuntimeError::FieldIndex(-1.0),
        RuntimeError::FieldCount,
        RuntimeError::InvalidUtf8,
        RuntimeError::EmptySeparator,
        RuntimeError::Regex("pattern failed".into()),
        RuntimeError::Format(Refusal::PrintfStar),
    ] {
        assert!(matches!(
            failure(AwkError::Runtime { error, record: 2.0 }),
            CommandFailure::Status {
                status: ExitCode::SYNTAX,
                ..
            }
        ));
    }
    let p = parsed("{print 1}");
    assert!(matches!(p.program.rules[0].action.kind, StmtKind::Block(_)));
}
