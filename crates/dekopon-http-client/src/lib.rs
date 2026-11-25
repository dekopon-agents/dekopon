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

pub fn client_builder(
    builder: reqwest::ClientBuilder,
    timeout: Duration,
    private_ca: Option<&[u8]>,
    user_agent: &str,
) -> Result<reqwest::ClientBuilder, reqwest::Error> {
    let mut builder = builder
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .retry(reqwest::retry::never())
        .timeout(timeout)
        .user_agent(user_agent);
    if let Some(pem) = private_ca {
        for certificate in reqwest::Certificate::from_pem_bundle(pem)? {
            builder = builder.add_root_certificate(certificate);
        }
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{net::TcpListener, sync::oneshot};

    #[tokio::test]
    async fn egress_defaults_override_proxy_redirect_and_retry_policies() {
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::clone(&calls);
        let retries = Arc::clone(&calls);
        let app = Router::new()
            .route(
                "/redirect",
                get(move |headers: HeaderMap| async move {
                    assert_eq!(headers["user-agent"], "caller/1.2.3");
                    requests.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::FOUND, [("location", "/retry")])
                }),
            )
            .route(
                "/retry",
                get(move || async move {
                    retries.fetch_add(1, Ordering::SeqCst);
                    StatusCode::SERVICE_UNAVAILABLE
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    stopped.await.unwrap();
                })
                .await
                .unwrap();
        });
        let builder = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .redirect(reqwest::redirect::Policy::limited(10))
            .retry(
                reqwest::retry::for_host("127.0.0.1")
                    .no_budget()
                    .classify_fn(|request| {
                        if request.status() == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE) {
                            request.retryable()
                        } else {
                            request.success()
                        }
                    }),
            );
        let builder =
            client_builder(builder, Duration::from_secs(2), None, "caller/1.2.3").unwrap();
        assert!(format!("{builder:?}").contains("timeout: 2s"));
        let client = builder.build().unwrap();
        assert_eq!(
            client
                .get(format!("http://{address}/redirect"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FOUND
        );
        assert_eq!(
            client
                .get(format!("http://{address}/retry"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
