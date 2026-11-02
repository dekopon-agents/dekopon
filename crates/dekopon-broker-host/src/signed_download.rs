use crate::{StoreState, asset::HandleResource};
use dekopon_http_host::{DownloadError, DownloadResponse, Window};

pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "signed-download-host",
        imports: { default: async | trappable },
        with: {
            "dekopon:asset/asset.handle": crate::asset::HandleResource,
            "dekopon:asset/asset.writer": crate::asset::WriterResource,
        },
    });
}

use bindings::dekopon::http0_1_0::signed_download as wit;

impl wit::Host for StoreState {
    async fn download(
        &mut self,
        request: wit::Request,
    ) -> wasmtime::Result<Result<wit::Response, wit::DownloadError>> {
        let Some(directory) = self.assets.directory()? else {
            self.assets
                .reject(crate::bindings::dekopon::asset::asset::ErrorCode::Unconfigured);
            return Ok(Err(wit::DownloadError {
                code: wit::ErrorCode::Internal,
            }));
        };
        let window = match request.window {
            wit::Window::Tail(length) => Window::Tail(length),
            wit::Window::Offset(window) => Window::Offset {
                start: window.start,
                length: window.length,
            },
        };
        let response = self
            .http
            .client
            .signed_download(request.api_uri, window, &directory)
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if self.http.client.asset_over_budget() {
                    self.assets
                        .reject(crate::bindings::dekopon::asset::asset::ErrorCode::OverBudget);
                }
                return Ok(Err(wit::DownloadError {
                    code: match error {
                        DownloadError::Denied => wit::ErrorCode::Denied,
                        DownloadError::RedirectRefused => wit::ErrorCode::RedirectRefused,
                        DownloadError::RequestLimit => wit::ErrorCode::RequestLimit,
                        DownloadError::Timeout => wit::ErrorCode::Timeout,
                        DownloadError::ResponseTooLarge(_) => wit::ErrorCode::ResponseTooLarge,
                        DownloadError::Incomplete => wit::ErrorCode::Incomplete,
                        DownloadError::Storage => wit::ErrorCode::Storage,
                        DownloadError::Internal => wit::ErrorCode::Internal,
                    },
                }));
            }
        };
        Ok(Ok(match response {
            DownloadResponse::Api(response) => wit::Response::Api(wit::ApiResponse {
                status: response.status,
                headers: response
                    .headers
                    .into_iter()
                    .map(|header| wit::Header {
                        name: header.name,
                        value: header.value,
                    })
                    .collect(),
                body: response.body,
            }),
            DownloadResponse::Storage {
                status,
                coverage,
                body,
            } => {
                let body = match body {
                    Some(body) => {
                        let handle = HandleResource::from_output(
                            body,
                            "text/plain".to_owned(),
                            crate::bindings::dekopon::asset::asset::Encoding::Identity,
                            "logs".to_owned(),
                        )
                        .await;
                        let handle = match handle {
                            Ok(handle) => handle,
                            Err(error) => {
                                self.assets.reject(error.code);
                                return Ok(Err(wit::DownloadError {
                                    code: wit::ErrorCode::Internal,
                                }));
                            }
                        };
                        Some(self.table.push(handle)?)
                    }
                    None => None,
                };
                wit::Response::Storage(wit::StorageResponse {
                    status,
                    body,
                    coverage: coverage.map(|coverage| wit::Coverage {
                        start: coverage.start,
                        end_exclusive: coverage.end_exclusive,
                        total: coverage.total,
                    }),
                })
            }
        }))
    }
}
