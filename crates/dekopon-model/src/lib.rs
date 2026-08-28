//! Bounded model transports and model-account authentication for Dekopon.
//!
//! This crate owns credentials used to authenticate model endpoints. Provider credentials remain
//! a separate concern and are never exposed through these clients.

#![forbid(unsafe_code)]

use std::time::Duration;

use ureq::{Agent, config::ConfigBuilder, typestate::AgentScope};

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
    agent_from(Agent::config_builder(), timeout)
}

/// Applies that stance to whatever configuration the caller started from.
///
/// Production always starts from `Agent::config_builder()`, which is `Config::default()` and its
/// ambient `Proxy::try_from_env()`. The seam exists for the proxy assertion: `try_from_env` answers
/// `None` unless a proxy variable is exported, so on a proxy-free runner a builder that had dropped
/// `.proxy(None)` would still produce an agent with no proxy and the test would prove nothing. The
/// test starts from a configuration that definitely carries one and watches this clear it, with no
/// process environment to mutate and nothing for a concurrent test to race.
fn agent_from(config: ConfigBuilder<AgentScope>, timeout: Duration) -> Agent {
    config
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use ureq::Proxy;

    use super::{Agent, AgentScope, ConfigBuilder, Duration, agent, agent_from};

    /// The discard port: a proxy that is well formed, never dialled, and obvious in a diff.
    const AMBIENT_PROXY: &str = "http://127.0.0.1:9";

    /// The shape `HTTPS_PROXY=http://127.0.0.1:9` would have left in `Config::default()`.
    fn proxied_configuration() -> ConfigBuilder<AgentScope> {
        Agent::config_builder().proxy(Some(
            Proxy::new(AMBIENT_PROXY).expect("a well-formed proxy uri"),
        ))
    }

    #[test]
    fn the_shared_agent_ignores_ambient_proxy_configuration() {
        // Not read from the environment: `Proxy::try_from_env()` answers `None` unless a proxy
        // variable is exported, so building from the default on a proxy-free runner asserts
        // nothing at all. Starting from a configuration that carries one is what makes the
        // assertion fail when `.proxy(None)` is missing — and it mutates no process state, so it
        // cannot race a concurrent test.
        assert!(
            proxied_configuration().build().proxy().is_some(),
            "the fixture must carry the proxy this test is about"
        );

        let agent = agent_from(proxied_configuration(), Duration::from_secs(30));

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
