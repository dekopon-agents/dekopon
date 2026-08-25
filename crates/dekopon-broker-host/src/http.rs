use std::time::Duration;

use dekopon_capability::{HttpConstraints, SecretUseGrant};
use dekopon_http_host::{
    BufferedHttpClient, ConfigurationError, ErrorCode as NativeErrorCode, Header as NativeHeader,
    HttpError as NativeHttpError, HttpHostCeilings, Request as NativeRequest,
};

use crate::bindings::dekopon::http::client::{ErrorCode, Header, HttpError, Request, Response};

pub use dekopon_http_host::{
    BoundCredential, ConfigurationError as HttpConfigurationError, HttpCallEvidence,
};

pub(crate) type HttpCeilings = HttpHostCeilings;

#[derive(Debug)]
pub(crate) struct HttpState {
    client: BufferedHttpClient,
}

impl HttpState {
    pub(crate) fn describe(
        ceilings: HttpCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        BufferedHttpClient::disabled(ceilings, timeout).map(|client| Self { client })
    }

    pub(crate) fn invoke(
        grant: Option<HttpConstraints>,
        secret_grant: Option<SecretUseGrant>,
        credential: Option<BoundCredential>,
        ceilings: HttpCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        let client = match (grant, secret_grant, credential) {
            (Some(grant), Some(secret), Some(credential)) => {
                BufferedHttpClient::authorized_with_secret_credential(
                    grant, secret, credential, ceilings, timeout,
                )?
            }
            (Some(grant), None, credential) => BufferedHttpClient::authorized_with_credential(
                grant, credential, ceilings, timeout,
            )?,
            (None, None, None) => BufferedHttpClient::disabled(ceilings, timeout)?,
            _ => {
                return Err(ConfigurationError::InvalidCredential {
                    reason: "HTTP grant, secret-use grant, and resolved credential disagree",
                });
            }
        };
        Ok(Self { client })
    }

    pub(crate) fn attempted(&self) -> bool {
        self.client.attempted()
    }

    pub(crate) fn policy_violation(&self) -> Option<&'static str> {
        self.client.policy_violation()
    }

    pub(crate) fn into_evidence(self) -> Vec<HttpCallEvidence> {
        self.client.into_evidence()
    }

    pub(crate) async fn send(&mut self, request: Request) -> Result<Response, HttpError> {
        self.client
            .send(NativeRequest {
                method: request.method,
                uri: request.uri,
                headers: request
                    .headers
                    .into_iter()
                    .map(|header| NativeHeader {
                        name: header.name,
                        value: header.value,
                    })
                    .collect(),
                body: request.body,
            })
            .await
            .map(|response| Response {
                status: response.status,
                headers: response
                    .headers
                    .into_iter()
                    .map(|header| Header {
                        name: header.name,
                        value: header.value,
                    })
                    .collect(),
                body: response.body,
            })
            .map_err(map_error)
    }
}

fn map_error(error: NativeHttpError) -> HttpError {
    HttpError {
        code: match error.code {
            NativeErrorCode::InvalidMethod => ErrorCode::InvalidMethod,
            NativeErrorCode::InvalidUri => ErrorCode::InvalidUri,
            NativeErrorCode::InvalidHeader => ErrorCode::InvalidHeader,
            NativeErrorCode::RequestTooLarge => ErrorCode::RequestTooLarge,
            NativeErrorCode::Denied => ErrorCode::Denied,
            NativeErrorCode::HostCallLimit => ErrorCode::HostCallLimit,
            NativeErrorCode::Dns => ErrorCode::Dns,
            NativeErrorCode::Connect => ErrorCode::Connect,
            NativeErrorCode::Tls => ErrorCode::Tls,
            NativeErrorCode::Timeout => ErrorCode::Timeout,
            NativeErrorCode::Protocol => ErrorCode::Protocol,
            NativeErrorCode::ResponseTooLarge => ErrorCode::ResponseTooLarge,
            NativeErrorCode::Internal => ErrorCode::Internal,
        },
        message: error.message,
    }
}

#[cfg(test)]
mod tests {
    use dekopon_http_host::{ErrorCode as NativeErrorCode, HttpError as NativeHttpError};

    use super::{ErrorCode, map_error};

    #[test]
    fn maps_every_native_error_class_to_the_versioned_wit_contract() {
        let cases = [
            (NativeErrorCode::InvalidMethod, ErrorCode::InvalidMethod),
            (NativeErrorCode::InvalidUri, ErrorCode::InvalidUri),
            (NativeErrorCode::InvalidHeader, ErrorCode::InvalidHeader),
            (NativeErrorCode::RequestTooLarge, ErrorCode::RequestTooLarge),
            (NativeErrorCode::Denied, ErrorCode::Denied),
            (NativeErrorCode::HostCallLimit, ErrorCode::HostCallLimit),
            (NativeErrorCode::Dns, ErrorCode::Dns),
            (NativeErrorCode::Connect, ErrorCode::Connect),
            (NativeErrorCode::Tls, ErrorCode::Tls),
            (NativeErrorCode::Timeout, ErrorCode::Timeout),
            (NativeErrorCode::Protocol, ErrorCode::Protocol),
            (
                NativeErrorCode::ResponseTooLarge,
                ErrorCode::ResponseTooLarge,
            ),
            (NativeErrorCode::Internal, ErrorCode::Internal),
        ];

        for (native, wit) in cases {
            let mapped = map_error(NativeHttpError {
                code: native,
                message: "bounded".to_owned(),
            });
            assert_eq!(mapped.code, wit);
            assert_eq!(mapped.message, "bounded");
        }
    }
}
