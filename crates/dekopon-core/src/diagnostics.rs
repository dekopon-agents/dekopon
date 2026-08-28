//! Rendering a failure so the reason survives the log line.

use std::error::Error;

/// Renders an error and its sources as one `a: b: c` line.
///
/// Every failure worth logging here is a wrapper whose own message names the layer rather than the
/// cause: a connection error says "broker failed", and the errno that says why is two levels down.
/// The chain is the diagnosable part, and dropping it is how a retrying loop ends up reporting
/// that something failed without ever saying what.
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
