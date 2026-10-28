use super::{ParseError, nesting_tests::two_mib, parse};
use proptest::{
    prelude::*,
    test_runner::{Config, RngAlgorithm, TestRng, TestRunner},
};

fn scripts() -> impl Strategy<Value = String> {
    let fragments = prop::sample::select(vec![
        "$(",
        "${",
        "$((",
        ")",
        "}",
        "))",
        "'",
        "\"",
        "\\",
        "\n",
        ";",
        "echo ",
        "x",
        "${x:-",
        "${x[",
        "${x#",
        "$(echo ",
        "<<EOF\n",
        "<<-EOF\n",
        "\nEOF\n",
        "case x in ",
        "x)",
        ";;",
        "esac",
        "[[ ",
        " ]]",
        "! ",
        " && ",
        " || ",
        " > ",
        "2>&1",
        "&>",
        "|",
        "1 + ",
        " ** ",
        "`",
        "#",
        "\0",
    ]);
    let text = prop::collection::vec(any::<char>(), 0..64)
        .prop_map(|chars| chars.into_iter().collect::<String>());
    let tokens = prop::collection::vec(fragments, 0..96).prop_map(|parts| parts.concat());
    let wrappers = prop::sample::select(vec![
        ("echo $(", ")"),
        ("echo ${x:-$(", ")}"),
        ("${x:-", "}"),
        ("echo \"$(", ")\""),
        ("cat <<EOF\n$(", ")\nEOF\n"),
        ("cat <<-EOF\n\t$(", ")\n\tEOF\n"),
        ("case x in x) ", ";; esac"),
        ("[[ ( ", " ) ]]"),
        ("{ ", "; } > out 2>&1"),
        ("if :; then ", "; fi"),
        ("echo $(( ", " ))"),
        ("\"", "\""),
        ("'", "'"),
    ]);
    let nested = (
        prop::collection::vec(wrappers, 0..96),
        prop_oneof![Just(":".to_owned()), text.clone(), tokens.clone()],
        any::<bool>(),
    )
        .prop_map(|(wrappers, mut script, truncate)| {
            for (open, close) in wrappers {
                script = format!("{open}{script}{close}");
            }
            if truncate {
                script.truncate(
                    script
                        .char_indices()
                        .nth(script.chars().count() / 2)
                        .map_or(0, |(i, _)| i),
                );
            }
            script
        });
    let arithmetic = (0..4_100usize, any::<bool>()).prop_map(|(depth, unary)| {
        let operator = if unary { "! " } else { "1 + " };
        format!("echo $(( {}1 ))", operator.repeat(depth))
    });
    prop_oneof![1 => text, 4 => tokens, 4 => nested, 1 => arithmetic].prop_map(|mut script| {
        let mut end = script.len().min(16 * 1024);
        while !script.is_char_boundary(end) {
            end -= 1;
        }
        script.truncate(end);
        script
    })
}

#[test]
fn generated_scripts_parse_or_return_a_typed_refusal_without_panicking_on_a_two_mib_stack() {
    two_mib(|| {
        let mut runner = TestRunner::new_with_rng(
            Config {
                cases: 2_048,
                max_shrink_iters: 256,
                failure_persistence: None,
                ..Config::default()
            },
            TestRng::from_seed(RngAlgorithm::ChaCha, &[42; 32]),
        );
        runner
            .run(&scripts(), |script| {
                match parse(&script) {
                    Ok(script) => drop(script),
                    Err(ParseError::Refused { refusal, .. }) => drop(refusal),
                    Err(ParseError::TooDeep { .. }) => (),
                }
                Ok(())
            })
            .expect("generated script must parse or return a typed refusal");
    });
}
