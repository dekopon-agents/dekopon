#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::time::Duration;

use ureq::{Agent, config::ConfigBuilder, typestate::AgentScope};

pub mod asset;

pub mod blocking;
pub mod chatgpt;
pub mod codex;
pub mod control;
mod diagnostic;
pub mod error;
mod http;
pub mod inference;
mod loopback;
#[cfg(test)]
mod mock;
pub mod model;
pub mod openai;
pub mod openrouter;
mod sse;
pub mod stream;
#[cfg(test)]
mod trace_capture;

pub use stream::{ModelText, TurnEvent, events_from_transcript};

/// Disables ureq's default ambient-proxy pickup and redirect-following, since an exported
/// HTTPS_PROXY would otherwise route the ChatGPT bearer token and OAuth exchange through an
/// unintended host.
pub(crate) fn agent(timeout: Duration) -> Agent {
    agent_from(Agent::config_builder(), timeout)
}

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

    const AMBIENT_PROXY: &str = "http://127.0.0.1:9";

    fn proxied_configuration() -> ConfigBuilder<AgentScope> {
        Agent::config_builder().proxy(Some(
            Proxy::new(AMBIENT_PROXY).expect("a well-formed proxy uri"),
        ))
    }

    #[test]
    fn the_shared_agent_ignores_ambient_proxy_configuration() {
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
