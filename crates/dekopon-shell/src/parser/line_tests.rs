use super::parse;

fn refusal_line(script: &str) -> usize {
    parse(script).expect_err("script is refused").line()
}

#[test]
fn command_substitution_refusal_has_absolute_line_three() {
    assert_eq!(refusal_line("\n\necho $(if true)"), 3);
}

#[test]
fn parameter_word_refusal_has_absolute_line_three() {
    assert_eq!(refusal_line("\n\necho ${x:-$(if true)}"), 3);
}

#[test]
fn here_document_body_refusal_has_absolute_line_three() {
    assert_eq!(refusal_line("cat <<E\n\n$(if true)\nE\n"), 3);
}

#[test]
fn parameter_index_refusal_has_absolute_line_three() {
    assert_eq!(refusal_line("\n\necho ${x[$(if true)]}"), 3);
}

#[test]
fn nested_arithmetic_refusal_has_absolute_line_three() {
    assert_eq!(refusal_line("\n\necho $(echo $(( 1 & 2 )))"), 3);
}

#[test]
fn arithmetic_in_a_multiline_word_uses_its_absolute_line() {
    assert_eq!(
        refusal_line("echo prefix\"first\nsecond\n$(( 1 & 2 ))\""),
        3
    );
}

#[test]
fn arithmetic_scanning_preserves_the_offending_characters_line() {
    assert_eq!(refusal_line("echo $(( &\n\n1 ))"), 1);
}

#[test]
fn each_pending_here_document_uses_its_actual_body_line() {
    assert_eq!(refusal_line("cat <<A <<B\nx\nA\n\n$(if true)\nB\n"), 5);
}
