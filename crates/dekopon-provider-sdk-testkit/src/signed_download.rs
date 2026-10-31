use std::time::Duration;

use dekopon_capability::{HttpConstraints, RequestTemplate, SignedDownload};
use dekopon_core::Redacted;
use dekopon_http_host::{
    BoundCredential, BufferedHttpClient, HttpHostCeilings, test_transport::Transport,
};
use dekopon_provider_sdk::{asset::Handle, signed_download as sdk};

use crate::{
    HttpScript,
    typed::{HTTP_BYTES, HarnessError},
};

#[derive(Clone)]
pub(crate) struct Scenario {
    pub origin: String,
    pub transport: Transport,
    hostname: String,
    api: Vec<HttpScript>,
    storage: HttpScript,
    maximum: u32,
}

impl Scenario {
    pub fn new(api: Vec<HttpScript>, storage: HttpScript, maximum: u32) -> Self {
        let hostname = api
            .first()
            .map(|step| step.hostname.clone())
            .unwrap_or_default();
        let transport = Transport::new(
            (
                hostname.clone(),
                api.iter()
                    .map(|step| native_response(&step.response))
                    .collect(),
            ),
            (storage.hostname.clone(), native_response(&storage.response)),
            maximum,
        );
        Self {
            origin: format!("https://{hostname}"),
            hostname,
            api,
            storage,
            maximum,
            transport,
        }
    }

    pub fn fresh(&self) -> Self {
        Self::new(self.api.clone(), self.storage.clone(), self.maximum)
    }

    pub fn validate(&self) -> Result<(), HarnessError> {
        for script in self.api.iter().chain([&self.storage]) {
            crate::typed::validate_response(&script.response)?;
            if script.method != "GET" || script.response.body.len() > HTTP_BYTES {
                return Err(HarnessError::Fixture(
                    "signed scripts require GET and bounded bodies",
                ));
            }
        }
        if self.api.iter().any(|step| step.hostname != self.hostname)
            || self
                .api
                .iter()
                .filter(|step| step.response.status == 302)
                .count()
                != 1
            || self.hostname == self.storage.hostname
        {
            return Err(HarnessError::Fixture(
                "signed script requires one API host with exactly one redirect to a separate host",
            ));
        }
        Ok(())
    }

    pub fn constraints(&self, templates: Vec<RequestTemplate>) -> HttpConstraints {
        HttpConstraints {
            allowed_hosts: vec![self.hostname.clone()],
            allowed_methods: vec!["GET".to_owned()],
            max_requests: self.maximum,
            max_request_bytes: HTTP_BYTES as u64,
            max_response_bytes: (HTTP_BYTES * 2) as u64,
            allow_plaintext_loopback: false,
            propagate_trace: false,
            signed_download: Some(SignedDownload::GithubActionsLogsV1),
            request_templates: templates,
        }
    }

    pub fn credential(&self) -> Result<BoundCredential, HarnessError> {
        BoundCredential::bearer(
            "Bearer",
            Redacted::new("testkit-synthetic-credential".to_owned()),
            vec![self.hostname.clone()],
        )
        .map_err(|_error| HarnessError::Fixture("invalid synthetic credential binding"))
    }

    pub fn client(&self) -> Result<BufferedHttpClient, HarnessError> {
        self.validate()?;
        BufferedHttpClient::authorized_with_credential(
            self.constraints(vec![]),
            Some(self.credential()?),
            HttpHostCeilings::default(),
            Duration::from_secs(10),
        )
        .map(|client| client.with_test_transport(self.transport.clone()))
        .map_err(|_error| HarnessError::Fixture("invalid signed-download grant"))
    }
}

fn native_response(
    response: &dekopon_provider_sdk::provider::Response,
) -> dekopon_http_host::Response {
    dekopon_http_host::Response {
        status: response.status,
        headers: response
            .headers
            .iter()
            .map(|header| dekopon_http_host::Header {
                name: header.name.clone(),
                value: header.value.clone(),
            })
            .collect(),
        body: response.body.clone(),
    }
}

pub(crate) fn host_request(
    request: dekopon_provider_sdk::provider::Request,
) -> dekopon_http_host::Request {
    dekopon_http_host::Request {
        method: request.method,
        uri: request.uri,
        headers: request
            .headers
            .into_iter()
            .map(|header| dekopon_http_host::Header {
                name: header.name,
                value: header.value,
            })
            .collect(),
        body: request.body,
    }
}

pub(crate) fn guest_response(
    response: dekopon_http_host::Response,
) -> dekopon_provider_sdk::provider::Response {
    dekopon_provider_sdk::provider::Response {
        status: response.status,
        headers: response
            .headers
            .into_iter()
            .map(|header| dekopon_provider_sdk::provider::Header {
                name: header.name,
                value: header.value,
            })
            .collect(),
        body: response.body,
    }
}

pub(crate) fn http_error(
    error: dekopon_http_host::HttpError,
) -> dekopon_provider_sdk::provider::HttpError {
    use dekopon_http_host::ErrorCode as E;
    use dekopon_provider_sdk::provider::HttpErrorCode as G;
    dekopon_provider_sdk::provider::HttpError {
        code: match error.code {
            E::InvalidMethod => G::InvalidMethod,
            E::InvalidUri => G::InvalidUri,
            E::InvalidHeader => G::InvalidHeader,
            E::RequestTooLarge => G::RequestTooLarge,
            E::Denied => G::Denied,
            E::HostCallLimit => G::HostCallLimit,
            E::Dns => G::Dns,
            E::Connect => G::Connect,
            E::Tls => G::Tls,
            E::Timeout => G::Timeout,
            E::Protocol => G::Protocol,
            E::ResponseTooLarge => G::ResponseTooLarge,
            E::Internal => G::Internal,
        },
        message: error.message,
    }
}

pub(crate) fn guest_request(
    request: dekopon_http_host::Request,
) -> dekopon_provider_sdk::provider::Request {
    dekopon_provider_sdk::provider::Request {
        method: request.method,
        uri: request.uri,
        headers: request
            .headers
            .into_iter()
            .map(|header| dekopon_provider_sdk::provider::Header {
                name: header.name,
                value: header.value,
            })
            .collect(),
        body: request.body,
    }
}

pub(crate) fn response(
    response: dekopon_http_host::DownloadResponse,
) -> Result<sdk::Response, sdk::DownloadError> {
    Ok(match response {
        dekopon_http_host::DownloadResponse::Api(response) => {
            sdk::Response::Api(sdk::ApiResponse {
                status: response.status,
                headers: response
                    .headers
                    .into_iter()
                    .map(|header| sdk::Header {
                        name: header.name,
                        value: header.value,
                    })
                    .collect(),
                body: response.body,
            })
        }
        dekopon_http_host::DownloadResponse::Storage {
            status,
            coverage,
            body,
        } => {
            let body = body
                .map(|body| {
                    use std::os::unix::fs::FileExt as _;
                    let mut bytes = vec![0; body.len() as usize];
                    body.file().read_exact_at(&mut bytes, 0).map_err(|_error| {
                        sdk::DownloadError {
                            code: sdk::ErrorCode::Internal,
                        }
                    })?;
                    Handle::from_test_bytes(bytes).map_err(|_error| sdk::DownloadError {
                        code: sdk::ErrorCode::Internal,
                    })
                })
                .transpose()?;
            sdk::Response::Storage {
                status,
                coverage: coverage.map(|coverage| sdk::Coverage {
                    start: coverage.start,
                    end_exclusive: coverage.end_exclusive,
                    total: coverage.total,
                }),
                body,
            }
        }
    })
}

pub(crate) fn error(error: dekopon_http_host::DownloadError) -> sdk::DownloadError {
    use dekopon_http_host::DownloadError as E;
    sdk::DownloadError {
        code: match error {
            E::Denied => sdk::ErrorCode::Denied,
            E::RedirectRefused => sdk::ErrorCode::RedirectRefused,
            E::RequestLimit => sdk::ErrorCode::RequestLimit,
            E::Timeout => sdk::ErrorCode::Timeout,
            E::ResponseTooLarge(_) => sdk::ErrorCode::ResponseTooLarge,
            E::Incomplete => sdk::ErrorCode::Incomplete,
            E::Storage => sdk::ErrorCode::Storage,
            E::Internal => sdk::ErrorCode::Internal,
        },
    }
}
