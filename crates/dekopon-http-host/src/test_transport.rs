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
    api_consumed: bool,
    guest: Option<Request>,
    requests: Vec<ObservedRequest>,
}

#[derive(Clone, Debug)]
pub struct Transport {
    scripts: Arc<[(String, Response); 2]>,
    maximum: u32,
    state: Arc<Mutex<State>>,
}

impl Transport {
    pub fn new(api: (String, Response), storage: (String, Response), maximum: u32) -> Self {
        Self {
            scripts: Arc::new([api, storage]),
            maximum,
            state: Arc::new(Mutex::new(State {
                api_consumed: false,
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
            || !self
                .scripts
                .iter()
                .any(|(host, _)| Some(host.as_str()) == url.host_str())
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
        let (index, (_, script)) = self
            .scripts
            .iter()
            .enumerate()
            .find(|(_, (host, _))| Some(host.as_str()) == url.host_str())
            .ok_or_else(|| http_error(ErrorCode::Denied, "destination has no test script"))?;
        let mut state = self.state.lock();
        if index == 0 && std::mem::replace(&mut state.api_consumed, true) {
            return Err(http_error(
                ErrorCode::Denied,
                "test API script already consumed",
            ));
        }
        if state.requests.len() >= self.maximum as usize {
            return Err(http_error(
                ErrorCode::HostCallLimit,
                "test request limit exhausted",
            ));
        }
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

    fn script(host: &str, status: u16) -> (String, Response) {
        (
            host.to_owned(),
            Response {
                status,
                headers: Vec::new(),
                body: b"body".to_vec(),
            },
        )
    }

    #[tokio::test]
    async fn the_api_script_answers_once_per_transport() {
        let transport = Transport::new(script("api.test", 302), script("storage.test", 200), 6);
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
        assert_eq!(client.send(get()).await.unwrap().status, 302);
        let error = client.send(get()).await.expect_err("api script is spent");
        assert_eq!(error.code, ErrorCode::Denied);
        assert_eq!(transport.requests().len(), 1);
    }
}
