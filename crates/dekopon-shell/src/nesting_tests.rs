use super::{ParseError, parse};

struct Case {
    name: &'static str,
    prefix: &'static str,
    open: &'static str,
    atom: &'static str,
    close: &'static str,
    suffix: &'static str,
    ceiling: usize,
    refusal: Refusal,
}

enum Refusal {
    Nesting(&'static str),
    Parameter,
    Arithmetic,
}

impl Case {
    fn script(&self, n: usize) -> String {
        format!(
            "{}{}{}{}{}",
            self.prefix,
            self.open.repeat(n),
            self.atom,
            self.close.repeat(n),
            self.suffix
        )
    }

    fn message(&self) -> String {
        match self.refusal {
            Refusal::Parameter => "parameter expansions nested deeper than 32".to_owned(),
            Refusal::Arithmetic => "an arithmetic expansion may nest at most 64 operators deep, counting each `!`, unary `-` and operator in a chain like `a + b + c`; split the calculation across assignments".to_owned(),
            Refusal::Nesting(construct) => format!("{construct} nested more than 64 levels deep; this shell parses on a fixed stack and refuses rather than risking it"),
        }
    }
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for (name, open, close, ceiling, construct) in [
        (
            "if condition",
            "if ",
            "; then :; fi",
            63,
            "an `if` statement",
        ),
        ("if body", "if :; then ", "; fi", 63, "an `if` statement"),
        (
            "elif condition",
            "if :; then :; elif ",
            "; then :; fi",
            63,
            "an `if` statement",
        ),
        (
            "while condition",
            "while ",
            "; do :; done",
            63,
            "a `while` loop",
        ),
        ("while body", "while :; do ", "; done", 63, "a `while` loop"),
        (
            "until condition",
            "until ",
            "; do :; done",
            63,
            "an `until` loop",
        ),
        (
            "until body",
            "until :; do ",
            "; done",
            63,
            "an `until` loop",
        ),
        ("for", "for x in a; do ", "; done", 63, "a `for` loop"),
        ("case", "case x in x) ", ";; esac", 63, "a `case` statement"),
        ("group", "{ ", "; }", 31, "a `{ ...; }` group"),
        ("function", "f() { ", "; }", 63, "a function body"),
        (
            "substitution",
            "echo $(",
            ")",
            31,
            "command substitution `$( ... )`",
        ),
    ] {
        cases.push(Case {
            name,
            prefix: "",
            open,
            atom: ":",
            close,
            suffix: "",
            ceiling,
            refusal: Refusal::Nesting(construct),
        });
    }
    cases.push(Case {
        name: "parameter",
        prefix: "echo ",
        open: "${x:-",
        atom: "x",
        close: "}",
        suffix: "",
        ceiling: 31,
        refusal: Refusal::Parameter,
    });
    for (name, prefix, open, close, suffix, ceiling, refusal) in [
        (
            "conditional not",
            "[[ ",
            "! ",
            "",
            " ]]",
            62,
            Refusal::Nesting("a `!` inside `[[ ... ]]`"),
        ),
        (
            "conditional group",
            "[[ ",
            "( ",
            " )",
            " ]]",
            62,
            Refusal::Nesting("a group inside `[[ ... ]]`"),
        ),
        (
            "arithmetic group",
            "echo $(( ",
            "( ",
            " )",
            " ))",
            63,
            Refusal::Nesting("an arithmetic expansion"),
        ),
        (
            "arithmetic unary",
            "echo $(( ",
            "! ",
            "",
            " ))",
            64,
            Refusal::Arithmetic,
        ),
        (
            "arithmetic binary",
            "echo $(( ",
            "1 + ",
            "",
            " ))",
            64,
            Refusal::Arithmetic,
        ),
    ] {
        cases.push(Case {
            name,
            prefix,
            open,
            atom: "1",
            close,
            suffix,
            ceiling,
            refusal,
        });
    }
    cases
}

fn refusal_message(error: ParseError) -> String {
    match error {
        ParseError::Syntax { line, message } => {
            assert_eq!(line, 1);
            message
        }
        ParseError::Lex(error) => {
            assert_eq!(error.line, 1);
            error.message
        }
    }
}

#[test]
fn each_production_preserves_its_measured_ceiling_and_refusal() {
    for case in cases() {
        parse(&case.script(case.ceiling))
            .unwrap_or_else(|e| panic!("{} at {}: {e}", case.name, case.ceiling));
        let error = parse(&case.script(case.ceiling + 1)).expect_err(case.name);
        assert_eq!(refusal_message(error), case.message(), "{}", case.name);
    }
}

fn two_mib<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    let [permit] = crate::test_threads().reserve().unwrap();
    std::thread::scope(|scope| {
        permit
            .spawn(
                scope,
                dekopon_process::ThreadName::new("sh-nesting-test"),
                f,
            )
            .expect("spawn parser thread")
            .join()
            .expect("join parser thread")
    })
}

#[test]
fn every_production_refuses_two_thousand_levels_on_a_two_mib_stack() {
    for case in cases() {
        let script = case.script(2_000);
        let error = two_mib(|| parse(&script)).expect_err(case.name);
        assert_eq!(refusal_message(error), case.message(), "{}", case.name);
    }
    let mut script = ":".to_owned();
    for _ in 0..2_000 {
        script = format!("echo $(echo ${{x:-$( {{ if :; then {script}; fi; }} )}})");
    }
    let error = two_mib(|| parse(&script)).expect_err("alternating nesting");
    match error {
        ParseError::Lex(_) | ParseError::Syntax { .. } => {}
    }
}

#[test]
fn for_words_case_subjects_and_each_pattern_lex_at_the_outer_depth() {
    for inner in [
        "for x in $(echo x); do :; done",
        "case $(echo x) in x) :;; esac",
        "case x in $(echo x)) :;; $(echo y)) :;; esac",
    ] {
        let script = format!("{}{inner}{}", "if :; then ".repeat(61), "; fi".repeat(61));
        parse(&script).expect("headers retain the outer depth");
    }
}
