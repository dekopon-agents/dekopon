use super::{Construct, ParseError, TooDeep, parse};

struct Case {
    name: &'static str,
    prefix: &'static str,
    open: &'static str,
    atom: &'static str,
    close: &'static str,
    suffix: &'static str,
    ceiling: usize,
    refusal: TooDeep,
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
            refusal: self.refusal.clone(),
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
            TooDeep::Grammar {
                construct: Construct::If,
            },
        ),
        (
            "if body",
            "if :; then ",
            "; fi",
            63,
            TooDeep::Grammar {
                construct: Construct::If,
            },
        ),
        (
            "elif condition",
            "if :; then :; elif ",
            "; then :; fi",
            63,
            TooDeep::Grammar {
                construct: Construct::If,
            },
        ),
        (
            "while condition",
            "while ",
            "; do :; done",
            63,
            TooDeep::Grammar {
                construct: Construct::While,
            },
        ),
        (
            "while body",
            "while :; do ",
            "; done",
            63,
            TooDeep::Grammar {
                construct: Construct::While,
            },
        ),
        (
            "until condition",
            "until ",
            "; do :; done",
            63,
            TooDeep::Grammar {
                construct: Construct::Until,
            },
        ),
        (
            "until body",
            "until :; do ",
            "; done",
            63,
            TooDeep::Grammar {
                construct: Construct::Until,
            },
        ),
        (
            "for",
            "for x in a; do ",
            "; done",
            63,
            TooDeep::Grammar {
                construct: Construct::For,
            },
        ),
        (
            "case",
            "case x in x) ",
            ";; esac",
            63,
            TooDeep::Grammar {
                construct: Construct::Case,
            },
        ),
        (
            "group",
            "{ ",
            "; }",
            31,
            TooDeep::Grammar {
                construct: Construct::Group,
            },
        ),
        (
            "function",
            "f() { ",
            "; }",
            63,
            TooDeep::Grammar {
                construct: Construct::Function,
            },
        ),
        (
            "substitution",
            "echo $(",
            ")",
            31,
            TooDeep::CommandSubstitution,
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
        refusal: TooDeep::Parameter,
    });
    for (name, prefix, open, close, suffix, ceiling, refusal) in [
        (
            "conditional not",
            "[[ ",
            "! ",
            "",
            " ]]",
            62,
            TooDeep::Grammar {
                construct: Construct::TestNot,
            },
        ),
        (
            "conditional group",
            "[[ ",
            "( ",
            " )",
            " ]]",
            62,
            TooDeep::Grammar {
                construct: Construct::TestGroup,
            },
        ),
        (
            "arithmetic group",
            "echo $(( ",
            "( ",
            " )",
            " ))",
            63,
            TooDeep::Grammar {
                construct: Construct::Arithmetic,
            },
        ),
        (
            "arithmetic unary",
            "echo $(( ",
            "! ",
            "",
            " ))",
            64,
            TooDeep::ArithmeticOperators,
        ),
        (
            "arithmetic binary",
            "echo $(( ",
            "1 + ",
            "",
            " ))",
            64,
            TooDeep::ArithmeticOperators,
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
        assert_eq!(error, case.error(), "{}", case.name);
    }
    let mut script = ":".to_owned();
    for _ in 0..2_000 {
        script = format!("echo $(echo ${{x:-$( {{ if :; then {script}; fi; }} )}})");
    }
    let error = two_mib(|| parse(&script)).expect_err("alternating nesting");
    assert_eq!(
        error,
        ParseError::TooDeep {
            line: 1,
            refusal: TooDeep::Parameter
        }
    );
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

#[test]
fn generated_scripts_parse_and_drop_without_panicking_on_a_two_mib_stack() {
    use arbitrary::Unstructured;
    use std::hash::{DefaultHasher, Hash, Hasher};

    two_mib(|| {
        let cases = cases();
        for case in 0_u32..1_024 {
            let mut bytes = [0_u8; 512];
            for (index, chunk) in bytes.chunks_exact_mut(8).enumerate() {
                let mut hash = DefaultHasher::new();
                (case, index).hash(&mut hash);
                chunk.copy_from_slice(&hash.finish().to_le_bytes());
            }
            let mut input = Unstructured::new(&bytes);
            let script = if (case as usize) < cases.len() {
                cases[case as usize].script(2_000)
            } else {
                generated_script(&mut input, &cases).expect("bounded generator has enough bytes")
            };
            match parse(&script) {
                Ok(program) => drop(program),
                Err(
                    ParseError::Lex { .. }
                    | ParseError::Refused { .. }
                    | ParseError::TooDeep { .. },
                ) => {}
            }
        }
    });
}

fn generated_script(
    input: &mut arbitrary::Unstructured<'_>,
    cases: &[Case],
) -> arbitrary::Result<String> {
    let fragments = [
        "$(",
        "${",
        "$((",
        "\"",
        "'",
        "cat <<E\n",
        "\nE\n",
        "case x in ",
        "x) :;; esac",
        "[[ ",
        " ]]",
        " >buf",
        "2>&1",
        "; ",
        "\n",
        "\\",
        "💠",
        "é",
        "\0",
        "echo ",
        "}",
        "))",
        ")",
        "${x:-",
        "$(echo ",
        "if :; then ",
        "; fi",
        "! ",
        "1 + ",
    ];
    match input.int_in_range(0_u8..=3)? {
        0 => {
            let mut script = String::new();
            for _ in 0..input.int_in_range(1..=64)? {
                script.push_str(input.choose(&fragments)?);
            }
            Ok(script)
        }
        1 => {
            let case = input.choose(cases)?;
            let depth = *input.choose(&[0, 1, 4, 16, 31, 32, 63, 64, 128, 2_000])?;
            Ok(case.script(depth))
        }
        2 => {
            let wrappers = [
                ("echo $(", ")"),
                ("echo ${x:-", "}"),
                ("if :; then ", "; fi"),
                ("case x in x) ", ";; esac"),
                ("{ ", "; }"),
                ("cat <<E\n", "\nE\n"),
                ("echo \"", "\""),
            ];
            let mut script = "echo é".to_owned();
            for _ in 0..input.int_in_range(0..=64)? {
                let (open, close) = input.choose(&wrappers)?;
                script = format!("{open}{script}{close}");
            }
            if input.arbitrary()? {
                let keep = input.int_in_range(0..=script.chars().count())?;
                let end = script
                    .char_indices()
                    .nth(keep)
                    .map_or(script.len(), |(index, _)| index);
                script.truncate(end);
            }
            Ok(script)
        }
        3 => {
            let mut script = "echo x".to_owned();
            for level in 0..input.int_in_range(0..=64)? {
                script = if input.arbitrary()? {
                    format!("echo $(echo ${{x:-{script}}})")
                } else {
                    format!("echo $(cat <<E{level}\n${{x:-$({script})}}\nE{level}\n)")
                };
            }
            Ok(script)
        }
        4..=u8::MAX => Err(arbitrary::Error::IncorrectFormat),
    }
}
