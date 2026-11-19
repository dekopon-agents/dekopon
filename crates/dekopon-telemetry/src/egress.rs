use std::time::Duration;

use reqwest::{Certificate, ClientBuilder, redirect, retry};

#[derive(Clone, Copy, Debug)]
pub enum Timeout {
    Request(Duration),
    Connect(Duration),
}

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
    };
    private_ca
        .into_iter()
        .fold(builder, ClientBuilder::add_root_certificate)
}
