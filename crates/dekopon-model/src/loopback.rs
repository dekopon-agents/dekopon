use crate::error::RequestError;

pub(crate) fn endpoint(value: &str) -> Result<String, RequestError> {
    let uri: http::Uri = value
        .parse()
        .map_err(|_invalid_uri| RequestError::InvalidLoopbackEndpoint)?;
    let authority = uri
        .authority()
        .ok_or(RequestError::InvalidLoopbackEndpoint)?;
    if uri.scheme_str() != Some("http")
        || !matches!(uri.host(), Some("127.0.0.1" | "[::1]"))
        || authority.as_str().contains('@')
        || value.contains('#')
        || reqwest::Url::parse(value).is_err()
    {
        return Err(RequestError::InvalidLoopbackEndpoint);
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_literal_ipv4_or_ipv6_loopback_over_plain_http_is_accepted() {
        for value in [
            "http://127.0.0.1:8000/responses",
            "http://127.0.0.1:65535/responses",
            "http://[::1]:8000/chat/completions",
        ] {
            assert_eq!(endpoint(value).unwrap(), value);
        }
        for value in [
            "",
            "127.0.0.1:8000",
            "http://localhost:8000",
            "https://127.0.0.1:8000",
            "http://127.0.0.2:8000",
            "http://[::2]:8000",
            "http://example.com",
            "http://127.0.0.1.example.com",
            "http://user@127.0.0.1:8000",
            "http://127.0.0.1:8000/#fragment",
            "http://2130706433:8000",
            "http://[::ffff:127.0.0.1]:8000",
            "file:///127.0.0.1",
            "http://127.0.0.1:65536/",
            "http://127.0.0.1:bad/",
            "http://[::1]:65536/",
        ] {
            assert!(
                matches!(endpoint(value), Err(RequestError::InvalidLoopbackEndpoint)),
                "{value}"
            );
        }
    }
}
