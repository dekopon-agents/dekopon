use std::error::Error;

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
