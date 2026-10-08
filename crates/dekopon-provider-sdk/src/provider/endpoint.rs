use std::borrow::Cow;
use std::fmt;

use serde::{Deserialize, Deserializer};

/// The origin, and an optional path prefix, a provider sends its requests to: the vendor's
/// default, or the owner's `providerSettings.<id>.baseUrl`. The broker's grant, not this type,
/// decides which destinations a call may reach.
///
/// ```compile_fail,E0080
/// const BAD: dekopon_provider_sdk::provider::endpoint::Base =
///     dekopon_provider_sdk::provider::endpoint::Base::from_static("ftp://example.com");
/// ```
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Base(Cow<'static, str>);

/// Why a base URL or a joined path was refused; the message never echoes the input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidBase {
    /// The scheme is not `http://` or `https://`.
    Scheme,
    /// The host is empty.
    Host,
    /// The authority carries userinfo (`user@`).
    Userinfo,
    /// The URL carries a query string.
    Query,
    /// The URL carries a fragment.
    Fragment,
    /// The URL contains whitespace.
    Whitespace,
    /// A joined path does not start with `/`.
    RelativePath,
}

impl InvalidBase {
    const fn message(self) -> &'static str {
        match self {
            Self::Scheme => "a base URL starts with http:// or https://",
            Self::Host => "a base URL names a host",
            Self::Userinfo => "a base URL carries no userinfo",
            Self::Query => "a base URL carries no query string",
            Self::Fragment => "a base URL carries no fragment",
            Self::Whitespace => "a base URL contains no whitespace",
            Self::RelativePath => "a joined path starts with /",
        }
    }
}

impl fmt::Display for InvalidBase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for InvalidBase {}

const fn checked(url: &str) -> Result<&str, InvalidBase> {
    let bytes = url.as_bytes();
    let authority = if let Some(rest) = strip(bytes, b"https://") {
        rest
    } else if let Some(rest) = strip(bytes, b"http://") {
        rest
    } else {
        return Err(InvalidBase::Scheme);
    };
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'?' => return Err(InvalidBase::Query),
            b'#' => return Err(InvalidBase::Fragment),
            byte if byte.is_ascii_whitespace() => return Err(InvalidBase::Whitespace),
            _ => {}
        }
        index += 1;
    }
    let mut index = 0;
    while index < authority.len() && authority[index] != b'/' {
        if authority[index] == b'@' {
            return Err(InvalidBase::Userinfo);
        }
        index += 1;
    }
    if index == 0 || authority[0] == b':' {
        return Err(InvalidBase::Host);
    }
    Ok(match url.as_bytes() {
        [.., b'/'] => url.split_at(url.len() - 1).0,
        _ => url,
    })
}

const fn strip<'a>(bytes: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if bytes.len() < prefix.len() {
        return None;
    }
    let mut index = 0;
    while index < prefix.len() {
        if bytes[index] != prefix[index] {
            return None;
        }
        index += 1;
    }
    Some(bytes.split_at(prefix.len()).1)
}

impl Base {
    /// Parses an `http://` or `https://` URL without userinfo, query, fragment or whitespace,
    /// trimming one trailing `/`.
    ///
    /// # Errors
    /// Returns the first rule `url` breaks.
    pub fn parse(url: &str) -> Result<Self, InvalidBase> {
        checked(url).map(|url| Self(Cow::Owned(url.to_owned())))
    }

    /// A provider's vendor default; an invalid `url` in a `const` is a compile error.
    #[must_use]
    pub const fn from_static(url: &'static str) -> Self {
        match checked(url) {
            Ok(url) => Self(Cow::Borrowed(url)),
            Err(invalid) => panic!("{}", invalid.message()),
        }
    }

    /// The base followed by `path`, which starts with `/`, so a path prefix in the base is kept
    /// exactly once.
    ///
    /// # Errors
    /// Refuses a `path` that does not start with `/`.
    pub fn join(&self, path: &str) -> Result<String, InvalidBase> {
        if path.starts_with('/') {
            Ok(format!("{}{path}", self.0))
        } else {
            Err(InvalidBase::RelativePath)
        }
    }

    /// The base URL, without a trailing `/`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Base {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Base {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let url = Cow::<'de, str>::deserialize(deserializer)?;
        Self::parse(&url).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::{Base, InvalidBase};

    #[test]
    fn parse_accepts_http_origins_with_prefixes_and_trims_one_slash() {
        for (url, base) in [
            ("https://api.example.com", "https://api.example.com"),
            ("https://api.example.com/", "https://api.example.com"),
            ("http://127.0.0.1:8787/exa/", "http://127.0.0.1:8787/exa"),
            ("http://[::1]:8787", "http://[::1]:8787"),
        ] {
            assert_eq!(
                Base::parse(url).map(|parsed| parsed.to_string()),
                Ok(base.to_owned())
            );
        }
        assert_eq!(
            Base::parse("https://x.example//").map(|base| base.to_string()),
            Ok("https://x.example/".to_owned())
        );
    }

    #[test]
    fn parse_refuses_what_would_confuse_the_vendor_or_the_broker() {
        for (url, invalid) in [
            ("ftp://example.com", InvalidBase::Scheme),
            ("HTTPS://example.com", InvalidBase::Scheme),
            ("example.com", InvalidBase::Scheme),
            ("https://", InvalidBase::Host),
            ("https:///path", InvalidBase::Host),
            ("https://:8080", InvalidBase::Host),
            ("http://:8080/prefix", InvalidBase::Host),
            ("https://user:secret@example.com", InvalidBase::Userinfo),
            ("https://example.com/?q=1", InvalidBase::Query),
            ("https://example.com/#top", InvalidBase::Fragment),
            ("https://example.com /", InvalidBase::Whitespace),
            ("https://example.com\n", InvalidBase::Whitespace),
        ] {
            assert_eq!(Base::parse(url), Err(invalid), "{url}");
        }
        assert!(Base::parse("https://example.com/users/@me").is_ok());
    }

    #[test]
    fn join_keeps_a_prefix_exactly_once_and_requires_a_leading_slash() {
        const VENDOR: Base = Base::from_static("https://api.example.com/");
        assert_eq!(VENDOR.as_str(), "https://api.example.com");
        assert_eq!(
            VENDOR.join("/search").as_deref(),
            Ok("https://api.example.com/search")
        );
        let fixture = Base::parse("http://127.0.0.1:8787/exa/").unwrap_or(VENDOR);
        assert_eq!(
            fixture.join("/search?x=1").as_deref(),
            Ok("http://127.0.0.1:8787/exa/search?x=1")
        );
        assert_eq!(fixture.join("search"), Err(InvalidBase::RelativePath));
        assert_eq!(fixture.join(""), Err(InvalidBase::RelativePath));
    }

    #[test]
    fn deserializing_an_invalid_base_fails_without_echoing_it() {
        let parsed = serde_json::from_str::<Base>(r#""https://example.com/""#);
        assert_eq!(parsed.ok(), Base::parse("https://example.com").ok());
        let refused = serde_json::from_str::<Base>(r#""https://secret@example.com""#)
            .err()
            .map(|error| error.to_string());
        assert!(refused.is_some_and(|message| !message.contains("secret")));
    }
}
