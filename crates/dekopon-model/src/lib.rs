//! Bounded model transports and model-account authentication for Dekopon.
//!
//! This crate owns credentials used to authenticate model endpoints. Provider credentials remain
//! a separate concern and are never exposed through these clients.

#![forbid(unsafe_code)]

use std::time::Duration;

use ureq::Agent;

/// Native ChatGPT/Codex subscription authentication and Responses transport.
pub mod chatgpt;
/// Bounded generated-image clients and output types.
pub mod image;
#[cfg(test)]
mod mock;
/// Generic chat-model contract and OpenAI-compatible transport.
pub mod model;

/// Builds the one HTTP agent shape every transport in this crate uses.
///
/// The four call sites differ only in their deadline, so the stance lives here rather than being
/// restated — and silently diverging — at each of them. None of the three settings is ureq's
/// default:
///
/// - `proxy(None)` overrides the `Proxy::try_from_env()` that `ureq`'s `Config::default()`
///   installs. Left at the default, an ambient `HTTPS_PROXY`/`ALL_PROXY` would carry the ChatGPT
///   bearer token, the OAuth device-code exchange, and every prompt through a host nobody named to
///   Dekopon. The native provider client in `dekopon-http-host` refuses ambient proxies for the
///   same reason, and this crate shares its process.
/// - `max_redirects(0)` keeps a credential-bearing request on the endpoint it was addressed to.
/// - A non-2xx must stay a response rather than becoming `Error::StatusCode`, whose `Display` is
///   only `http status: 429`. The endpoint's own JSON — model not found, context length, which
///   rate limit — is the entire diagnostic and is otherwise dropped.
pub(crate) fn agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::{Duration, agent};

    #[test]
    fn the_shared_agent_ignores_ambient_proxy_configuration() {
        // `Config::default()` reads `HTTPS_PROXY`/`ALL_PROXY`/`HTTP_PROXY` at build time, so this
        // is the assertion that a builder which forgot `.proxy(None)` would fail — no environment
        // mutation, and no race with any other test.
        let agent = agent(Duration::from_secs(30));

        assert!(
            agent.config().proxy().is_none(),
            "model transports must not inherit an ambient proxy"
        );
    }

    #[test]
    fn the_shared_agent_keeps_redirects_off_and_statuses_readable() {
        let agent = agent(Duration::from_secs(30));

        assert_eq!(agent.config().max_redirects(), 0);
        assert!(!agent.config().http_status_as_error());
    }
}
