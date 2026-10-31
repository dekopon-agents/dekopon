use crate::asset::Handle;

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "signed-download-client",
        with: {
            "dekopon:asset/asset@0.1.0": crate::asset::bindings::dekopon::asset::asset,
        },
        generate_all,
    });
}

use bindings::dekopon::http0_1_0::signed_download as wit;
pub use wit::{
    ApiResponse, Coverage, DownloadError, ErrorCode, Header, OffsetWindow, Request, Window,
};

pub enum Response {
    Api(ApiResponse),
    Storage {
        status: u16,
        coverage: Option<Coverage>,
        body: Option<Handle>,
    },
}

#[cfg(target_arch = "wasm32")]
pub fn download(request: &Request) -> Result<Response, DownloadError> {
    wit::download(request).map(|response| match response {
        wit::Response::Api(response) => Response::Api(response),
        wit::Response::Storage(response) => Response::Storage {
            status: response.status,
            coverage: response.coverage,
            body: response.body.map(Handle::from_inner),
        },
    })
}

#[cfg(all(not(target_arch = "wasm32"), not(feature = "native-test")))]
pub fn download(_request: &Request) -> Result<Response, DownloadError> {
    Err(DownloadError {
        code: ErrorCode::Denied,
    })
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-test"))]
pub fn download(request: &Request) -> Result<Response, DownloadError> {
    crate::provider::native_signed_download(request)
}
