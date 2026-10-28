use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::{Method, Url, header::HeaderMap};

use crate::{ErrorCode, Header, HttpError, Request, Response, http_error};

/// One request the scripted transport served.
#[derive(Clone, Debug)]
pub struct ObservedRequest {
    /// The request as the guest built it, before the host injected any credential or range.
    pub guest: Request,
    /// The request as the host transmitted it, including injected credentials and headers.
    pub sent: Request,
}

#[derive(Debug)]
struct State {
    api_next: usize,
    guest: Option<Request>,
    requests: Vec<ObservedRequest>,
}

#[derive(Clone, Debug)]
pub struct Transport {
    api: Arc<(String, Box<[Response]>)>,
    storage: Arc<(String, Response)>,
    maximum: u32,
    state: Arc<Mutex<State>>,
}

impl Transport {
    pub fn new(api: (String, Vec<Response>), storage: (String, Response), maximum: u32) -> Self {
        Self {
            api: Arc::new((api.0, api.1.into_boxed_slice())),
            storage: Arc::new(storage),
            maximum,
            state: Arc::new(Mutex::new(State {
                api_next: 0,
                guest: None,
                requests: Vec::new(),
            })),
        }
    }

    pub fn requests(&self) -> Vec<ObservedRequest> {
        self.state.lock().requests.clone()
    }

    pub(super) fn admit(&self, url: &Url) -> Result<(), HttpError> {
        if url.scheme() != "https"
            || url.port_or_known_default() != Some(443)
            || ![self.api.0.as_str(), self.storage.0.as_str()]
                .contains(&url.host_str().unwrap_or_default())
        {
            return Err(http_error(
                ErrorCode::Denied,
                "destination has no test script",
            ));
        }
        Ok(())
    }

    pub(super) fn guest(&self, request: Request) {
        self.state.lock().guest = Some(request);
    }

    pub(super) fn response(
        &self,
        method: &Method,
        url: &Url,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<reqwest::Response, HttpError> {
        self.admit(url)?;
        let mut state = self.state.lock();
        if state.requests.len() >= self.maximum as usize {
            return Err(http_error(
                ErrorCode::HostCallLimit,
                "test request limit exhausted",
            ));
        }
        let script = if Some(self.api.0.as_str()) == url.host_str() {
            let script =
                self.api.1.get(state.api_next).ok_or_else(|| {
                    http_error(ErrorCode::Denied, "test API script already consumed")
                })?;
            state.api_next += 1;
            script
        } else {
            &self.storage.1
        };
        let guest = state
            .guest
            .take()
            .ok_or_else(|| http_error(ErrorCode::Internal, "missing test guest request"))?;
        state.requests.push(ObservedRequest {
            guest,
            sent: Request {
                method: method.to_string(),
                uri: url.as_str().to_owned(),
                headers: headers
                    .iter()
                    .map(|(name, value)| Header {
                        name: name.to_string(),
                        value: value.as_bytes().to_vec(),
                    })
                    .collect(),
                body: body.to_vec(),
            },
        });
        drop(state);
        let mut response = http::Response::builder().status(script.status);
        for header in &script.headers {
            response = response.header(&header.name, header.value.as_slice());
        }
        let mut bytes = script.body.as_slice();
        if script.status == 200
            && let Some(range) = headers.get("range")
        {
            let (start, end) = range
                .to_str()
                .ok()
                .and_then(|range| range.strip_prefix("bytes="))
                .and_then(|range| range.split_once('-'))
                .and_then(|(start, end)| {
                    Some((start.parse::<usize>().ok()?, end.parse::<usize>().ok()?))
                })
                .ok_or_else(|| http_error(ErrorCode::Protocol, "invalid test range"))?;
            bytes = end
                .checked_add(1)
                .and_then(|end| script.body.get(start..end))
                .ok_or_else(|| http_error(ErrorCode::Protocol, "test range outside body"))?;
            response = response.status(206).header(
                "content-range",
                format!("bytes {start}-{end}/{}", script.body.len()),
            );
        }
        response
            .body(bytes.to_vec())
            .map(reqwest::Response::from)
            .map_err(|_error| http_error(ErrorCode::Protocol, "invalid test response"))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dekopon_capability::HttpConstraints;

    use super::*;
    use crate::{BufferedHttpClient, HttpHostCeilings};

    fn response(status: u16) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: b"body".to_vec(),
        }
    }

    #[tokio::test]
    async fn api_steps_answer_in_order_and_each_only_once() {
        let transport = Transport::new(
            ("api.test".to_owned(), vec![response(200), response(302)]),
            ("storage.test".to_owned(), response(200)),
            6,
        );
        let mut client = BufferedHttpClient::authorized(
            HttpConstraints {
                allowed_hosts: vec!["api.test".to_owned()],
                allowed_methods: vec!["GET".to_owned()],
                max_requests: 6,
                max_request_bytes: 64 * 1024,
                max_response_bytes: 64 * 1024,
                allow_plaintext_loopback: false,
                propagate_trace: false,
                request_templates: Vec::new(),
                signed_download: None,
            },
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .unwrap()
        .with_test_transport(transport.clone());
        let get = || Request {
            method: "GET".to_owned(),
            uri: "https://api.test/log".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert_eq!(client.send(get()).await.unwrap().status, 200);
        assert_eq!(client.send(get()).await.unwrap().status, 302);
        let error = client.send(get()).await.expect_err("api script is spent");
        assert_eq!(error.code, ErrorCode::Denied);
        assert_eq!(transport.requests().len(), 2);
    }
}
