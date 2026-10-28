use super::{ParseError, errors::DepthConstruct, nested::Construct, parse};

struct Case {
    name: &'static str,
    prefix: &'static str,
    open: &'static str,
    atom: &'static str,
    close: &'static str,
    suffix: &'static str,
    ceiling: usize,
    refusal: DepthConstruct,
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

    fn error(&self) -> ParseError {
        ParseError::TooDeep {
            line: 1,
            construct: self.refusal,
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
            DepthConstruct::Block(Construct::If),
        ),
        (
            "if body",
            "if :; then ",
            "; fi",
            63,
            DepthConstruct::Block(Construct::If),
        ),
        (
            "elif condition",
            "if :; then :; elif ",
            "; then :; fi",
            63,
            DepthConstruct::Block(Construct::If),
        ),
        (
            "while condition",
            "while ",
            "; do :; done",
            63,
            DepthConstruct::Block(Construct::While),
        ),
        (
            "while body",
            "while :; do ",
            "; done",
            63,
            DepthConstruct::Block(Construct::While),
        ),
        (
            "until condition",
            "until ",
            "; do :; done",
            63,
            DepthConstruct::Block(Construct::Until),
        ),
        (
            "until body",
            "until :; do ",
            "; done",
            63,
            DepthConstruct::Block(Construct::Until),
        ),
        (
            "for",
            "for x in a; do ",
            "; done",
            63,
            DepthConstruct::Block(Construct::For),
        ),
        (
            "case",
            "case x in x) ",
            ";; esac",
            63,
            DepthConstruct::Block(Construct::Case),
        ),
        (
            "group",
            "{ ",
            "; }",
            31,
            DepthConstruct::Block(Construct::Group),
        ),
        (
            "function",
            "f() { ",
            "; }",
            63,
            DepthConstruct::Block(Construct::Function),
        ),
        (
            "substitution",
            "echo $(",
            ")",
            31,
            DepthConstruct::Substitution,
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
            refusal: construct,
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
        refusal: DepthConstruct::Parameter,
    });
    for (name, prefix, open, close, suffix, ceiling, refusal) in [
        (
            "conditional not",
            "[[ ",
            "! ",
            "",
            " ]]",
            62,
            DepthConstruct::Block(Construct::TestNot),
        ),
        (
            "conditional group",
            "[[ ",
            "( ",
            " )",
            " ]]",
            62,
            DepthConstruct::Block(Construct::TestGroup),
        ),
        (
            "arithmetic group",
            "echo $(( ",
            "( ",
            " )",
            " ))",
            63,
            DepthConstruct::ArithmeticGroup,
        ),
        (
            "arithmetic unary",
            "echo $(( ",
            "! ",
            "",
            " ))",
            64,
            DepthConstruct::ArithmeticOperators,
        ),
        (
            "arithmetic binary",
            "echo $(( ",
            "1 + ",
            "",
            " ))",
            64,
            DepthConstruct::ArithmeticOperators,
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

#[test]
fn each_production_preserves_its_measured_ceiling_and_refusal() {
    for case in cases() {
        parse(&case.script(case.ceiling))
            .unwrap_or_else(|e| panic!("{} at {}: {e}", case.name, case.ceiling));
        let error = parse(&case.script(case.ceiling + 1)).expect_err(case.name);
        assert_eq!(error, case.error(), "{}", case.name);
    }
}

pub(super) fn two_mib<T: Send>(f: impl FnOnce() -> T + Send) -> T {
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
        assert_eq!(error, case.error(), "{}", case.name);
    }
    let mut script = ":".to_owned();
    for _ in 0..2_000 {
        script = format!("echo $(echo ${{x:-$( {{ if :; then {script}; fi; }} )}})");
    }
    let error = two_mib(|| parse(&script)).expect_err("alternating nesting");
    match error {
        ParseError::Refused { .. } | ParseError::TooDeep { .. } => {}
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
