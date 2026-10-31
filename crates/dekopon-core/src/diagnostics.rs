use std::error::Error;

use crate::attribute::TRUNCATION_MARKER;

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

    use super::{MAX_PROBLEM_BYTES, bounded_problems, error_chain};

    #[derive(Debug, thiserror::Error)]
    #[error("accepting a connection failed")]
    struct Wrapper(#[source] io::Error);

    #[test]
    fn refusal_renders_eight_bounded_problems_and_the_remaining_count() {
        let long = "界".repeat(300);
        let problems = std::iter::once(long)
            .chain((1..10).map(|index| format!("problem #{index}")))
            .collect::<Vec<_>>();
        let rendered = bounded_problems(problems.into_iter());
        let parts = rendered.split("; ").collect::<Vec<_>>();
        assert_eq!(parts.len(), 9);
        assert!(parts[0].len() <= MAX_PROBLEM_BYTES);
        assert!(parts[0].ends_with("…[truncated]"));
        assert_eq!(parts[7], "problem #7");
        assert_eq!(parts[8], "and 2 more");
    }

    #[test]
    fn renders_the_wrapper_and_its_cause() {
        let rendered = error_chain(&Wrapper(io::Error::other("too many open files")));
        assert_eq!(
            rendered,
            "accepting a connection failed: too many open files"
        );
    }
}
