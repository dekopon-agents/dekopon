use std::time::Duration;

use reqwest::{Certificate, ClientBuilder, redirect, retry};

#[derive(Clone, Copy, Debug)]
pub enum Timeout {
    Request(Duration),
    Connect(Duration),
    Streaming { connect: Duration, read: Duration },
}

#[expect(clippy::todo, reason = "skeleton: daemon/egress-client")]
pub fn client_builder(
    builder: ClientBuilder,
    timeout: Timeout,
    user_agent: &'static str,
    private_ca: impl IntoIterator<Item = Certificate>,
) -> ClientBuilder {
    let builder = builder
        .redirect(redirect::Policy::none())
        .no_proxy()
        .retry(retry::never())
        .user_agent(user_agent);
    let builder = match timeout {
        Timeout::Request(timeout) => builder.timeout(timeout),
        Timeout::Connect(timeout) => builder.connect_timeout(timeout),
        Timeout::Streaming { .. } => todo!("skeleton: daemon/egress-client"),
    };
    private_ca
        .into_iter()
        .fold(builder, ClientBuilder::add_root_certificate)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore = "skeleton: daemon/egress-client"]
    #[expect(clippy::todo, reason = "skeleton: daemon/egress-client")]
    async fn a_stalled_upstream_ends_at_the_read_timeout() {
        todo!("skeleton: daemon/egress-client")
    }
}
