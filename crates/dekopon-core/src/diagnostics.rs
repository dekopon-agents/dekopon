use std::error::Error;

use crate::attribute::TRUNCATION_MARKER;

/// The top message only names the layer, not the cause; dropping the chain is how a retry loop logs
/// a failure with no reason.
pub fn error_chain(error: &dyn Error) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(current) = source {
        rendered.push_str(": ");
        rendered.push_str(&current.to_string());
        source = current.source();
    }
    rendered
}

pub const MAX_RENDERED_PROBLEMS: usize = 8;
pub const MAX_PROBLEM_BYTES: usize = 512;

#[must_use]
pub fn bounded_problems(problems: impl ExactSizeIterator<Item = String>) -> String {
    let total = problems.len();
    let mut rendered = problems
        .take(MAX_RENDERED_PROBLEMS)
        .map(|problem| {
            if problem.len() <= MAX_PROBLEM_BYTES {
                return problem;
            }
            let cut = problem.floor_char_boundary(MAX_PROBLEM_BYTES - TRUNCATION_MARKER.len());
            format!("{}{TRUNCATION_MARKER}", &problem[..cut])
        })
        .collect::<Vec<_>>();
    if let Some(more) = total
        .checked_sub(MAX_RENDERED_PROBLEMS)
        .filter(|more| *more > 0)
    {
        rendered.push(format!("and {more} more"));
    }
    rendered.join("; ")
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::error_chain;

    #[derive(Debug, thiserror::Error)]
    #[error("accepting a connection failed")]
    struct Wrapper(#[source] io::Error);

    #[test]
    fn renders_the_wrapper_and_its_cause() {
        let rendered = error_chain(&Wrapper(io::Error::other("too many open files")));
        assert_eq!(
            rendered,
            "accepting a connection failed: too many open files"
        );
    }
}
