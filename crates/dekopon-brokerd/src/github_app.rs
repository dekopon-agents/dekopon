use std::{collections::BTreeMap, num::NonZeroU64};

use async_trait::async_trait;
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use dekopon_broker::{CredentialRefreshError, RefreshingCredential};
use dekopon_broker_host::BoundCredential;
use dekopon_core::Redacted;
use reqwest::{
    StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue},
    redirect,
};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::Instrument as _;

use crate::credentials::REFRESH_TIMEOUT;

pub(crate) const HARD_MAX_GITHUB_APP_KEY_BYTES: usize = 16 * 1024;
const GITHUB_API_BASE: &str = "https://api.github.com";
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
// GitHub refuses an App JWT whose `exp` is more than ten minutes out, and backdating `iat` absorbs
// clock drift between the broker and GitHub.
const JWT_BACKDATE: time::Duration = time::Duration::seconds(60);
const JWT_LIFETIME: time::Duration = time::Duration::minutes(9);
const RENEW_BEFORE_EXPIRY: time::Duration = time::Duration::minutes(5);

#[derive(Debug, Error)]
pub(crate) enum GithubAppError {
    #[error("privateKey is not a PEM-encoded RSA private key")]
    NotPem,
    #[error("privateKey is not a usable RSA key ({reason})")]
    KeyRejected { reason: String },
    #[error("the GitHub API client could not be built")]
    Client,
}

pub(crate) struct GithubAppKey(RsaKeyPair);

impl GithubAppKey {
    pub(crate) fn from_pem(pem: &[u8]) -> Result<Self, GithubAppError> {
        let text = std::str::from_utf8(pem).map_err(|_utf8| GithubAppError::NotPem)?;
        let (label, body) = pem_body(text).ok_or(GithubAppError::NotPem)?;
        let der = STANDARD
            .decode(body)
            .map_err(|_decode| GithubAppError::NotPem)?;
        let parsed = match label {
            PemLabel::Pkcs1 => RsaKeyPair::from_der(&der),
            PemLabel::Pkcs8 => RsaKeyPair::from_pkcs8(&der),
        };
        parsed
            .map(Self)
            .map_err(|rejected| GithubAppError::KeyRejected {
                reason: rejected.to_string(),
            })
    }
}

enum PemLabel {
    Pkcs1,
    Pkcs8,
}

fn pem_body(text: &str) -> Option<(PemLabel, String)> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .skip_while(|line| line.is_empty());
    let label = match lines.next()? {
        "-----BEGIN RSA PRIVATE KEY-----" => PemLabel::Pkcs1,
        "-----BEGIN PRIVATE KEY-----" => PemLabel::Pkcs8,
        _ => return None,
    };
    let end = match label {
        PemLabel::Pkcs1 => "-----END RSA PRIVATE KEY-----",
        PemLabel::Pkcs8 => "-----END PRIVATE KEY-----",
    };
    let mut body = String::with_capacity(text.len());
    for line in lines {
        if line == end {
            return Some((label, body));
        }
        body.push_str(line);
    }
    None
}

#[derive(Debug, Serialize)]
pub(crate) struct Downscope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) repositories: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) permissions: Option<BTreeMap<String, String>>,
}

#[derive(Debug)]
pub(crate) struct Installation {
    pub(crate) app_id: NonZeroU64,
    pub(crate) installation_id: NonZeroU64,
    pub(crate) downscope: Option<Downscope>,
}

pub(crate) struct GithubAppCredential {
    name: String,
    app_id: NonZeroU64,
    key: GithubAppKey,
    endpoint: String,
    body: Option<Vec<u8>>,
    destinations: Vec<String>,
    client: reqwest::Client,
    // Held across the mint so concurrent resolutions share one installation token.
    cached: tokio::sync::Mutex<Option<CachedToken>>,
}

struct CachedToken {
    token: Redacted<String>,
    renew_at: OffsetDateTime,
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    token: Redacted<String>,
    expires_at: String,
}

impl std::fmt::Debug for GithubAppCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GithubAppCredential")
            .field("name", &self.name)
            .field("app_id", &self.app_id)
            .field("endpoint", &self.endpoint)
            .field("destinations", &self.destinations)
            .finish_non_exhaustive()
    }
}

impl GithubAppCredential {
    pub(crate) fn new(
        name: String,
        installation: Installation,
        key: GithubAppKey,
        destinations: Vec<String>,
    ) -> Result<Self, GithubAppError> {
        Self::with_api_base(name, installation, key, destinations, GITHUB_API_BASE)
    }

    pub(crate) fn with_api_base(
        name: String,
        installation: Installation,
        key: GithubAppKey,
        destinations: Vec<String>,
        api_base: &str,
    ) -> Result<Self, GithubAppError> {
        let body = installation
            .downscope
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_serialize| GithubAppError::Client)?;
        let client = reqwest::Client::builder()
            .redirect(redirect::Policy::none())
            .no_proxy()
            .timeout(REFRESH_TIMEOUT)
            .user_agent(concat!("dekopon-brokerd/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_build| GithubAppError::Client)?;
        Ok(Self {
            name,
            app_id: installation.app_id,
            key,
            endpoint: format!(
                "{}/app/installations/{}/access_tokens",
                api_base.trim_end_matches('/'),
                installation.installation_id
            ),
            body,
            destinations,
            client,
            cached: tokio::sync::Mutex::new(None),
        })
    }

    fn bind(&self, token: &Redacted<String>) -> Result<BoundCredential, CredentialRefreshError> {
        BoundCredential::bearer("Bearer", token.clone(), self.destinations.clone()).map_err(
            |source| {
                self.failed("invalid-material", Some(&source));
                CredentialRefreshError::Unavailable {
                    category: "invalid-material",
                }
            },
        )
    }

    fn jwt(&self, now: OffsetDateTime) -> Result<Redacted<String>, CredentialRefreshError> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "iat": (now - JWT_BACKDATE).unix_timestamp(),
                "exp": (now + JWT_LIFETIME).unix_timestamp(),
                "iss": self.app_id.get(),
            })
            .to_string(),
        );
        let signing_input = format!("{header}.{claims}");
        let mut signature = vec![0; self.key.0.public().modulus_len()];
        self.key
            .0
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_unspecified| {
                self.failed("signing", None);
                CredentialRefreshError::Unavailable {
                    category: "signing",
                }
            })?;
        Ok(Redacted::new(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        )))
    }

    async fn mint(&self, now: OffsetDateTime) -> Result<CachedToken, CredentialRefreshError> {
        let jwt = self.jwt(now)?;
        let mut authorization = HeaderValue::try_from(format!("Bearer {}", jwt.expose()))
            .map_err(|_invalid| self.unavailable("signing"))?;
        authorization.set_sensitive(true);
        let mut request = self
            .client
            .post(&self.endpoint)
            .header(AUTHORIZATION, authorization)
            .header(ACCEPT, "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        if let Some(body) = &self.body {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(body.clone());
        }
        let mut response = request
            .send()
            .await
            .map_err(|_transport| self.unavailable("transport"))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            tracing::error!(
                event = "broker_github_app_credential_reauth_required",
                credential = %self.name,
                http.response.status_code = status.as_u16(),
                "GitHub refused the App JWT; the App's private key or installation must be renewed"
            );
            return Err(CredentialRefreshError::ReauthorizationRequired);
        }
        if status.is_server_error() {
            return Err(self.unavailable("token-endpoint-unavailable"));
        }
        if !status.is_success() {
            return Err(self.unavailable("token-endpoint-rejected"));
        }
        let mut body = Vec::with_capacity(4096);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_transport| self.unavailable("transport"))?
        {
            if body.len() + chunk.len() > MAX_TOKEN_RESPONSE_BYTES {
                return Err(self.unavailable("token-endpoint-protocol"));
            }
            body.extend_from_slice(&chunk);
        }
        let minted = serde_json::from_slice::<AccessTokenResponse>(&body)
            .map_err(|_decode| self.unavailable("token-endpoint-protocol"))?;
        let expires_at = OffsetDateTime::parse(&minted.expires_at, &Rfc3339)
            .map_err(|_parse| self.unavailable("token-endpoint-protocol"))?;
        Ok(CachedToken {
            token: minted.token,
            renew_at: expires_at - RENEW_BEFORE_EXPIRY,
        })
    }

    fn unavailable(&self, category: &'static str) -> CredentialRefreshError {
        self.failed(category, None);
        CredentialRefreshError::Unavailable { category }
    }

    fn failed(&self, category: &'static str, reason: Option<&dyn std::error::Error>) {
        tracing::warn!(
            event = "broker_github_app_credential_refresh_failed",
            credential = %self.name,
            category = category,
            reason = reason.map(tracing::field::display),
            "a GitHub App installation token could not be minted"
        );
    }
}

#[async_trait]
impl RefreshingCredential for GithubAppCredential {
    fn destinations(&self) -> &[String] {
        &self.destinations
    }

    async fn resolve(&self) -> Result<BoundCredential, CredentialRefreshError> {
        let mut cached = self.cached.lock().await;
        let now = OffsetDateTime::now_utc();
        if let Some(current) = cached.as_ref()
            && now < current.renew_at
        {
            return self.bind(&current.token);
        }
        let span = tracing::info_span!(
            "broker.credential.refresh",
            credential = %self.name,
            outcome = tracing::field::Empty,
        );
        let minted = match self.mint(now).instrument(span.clone()).await {
            Ok(minted) => minted,
            Err(error) => {
                span.record("outcome", "failed");
                return Err(error);
            }
        };
        span.record("outcome", "renewed");
        let bound = self.bind(&minted.token)?;
        *cached = Some(minted);
        Ok(bound)
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU64, sync::Arc};

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use dekopon_broker::{CredentialRefreshError, RefreshingCredential as _};
    use dekopon_test_support::LoopbackServer;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};

    use super::{Downscope, GithubAppCredential, GithubAppKey, Installation};

    const KEY_PEM: &[u8] = include_bytes!("../tests/fixture/github-app-test-only.pem");
    const PUBLIC_KEY_DER: &[u8] =
        include_bytes!("../tests/fixture/github-app-test-only.public.der");
    const APP_ID: u64 = 1_000_001;
    const INSTALLATION_ID: u64 = 2_000_002;
    const TOKEN: &str = "ghs_fixtureInstallationTokenValue0001";

    fn token_response(token: &str, expires_in: time::Duration) -> Vec<u8> {
        let expires_at = (OffsetDateTime::now_utc() + expires_in)
            .format(&Rfc3339)
            .expect("format expiry");
        status_response(
            "201 Created",
            &serde_json::json!({
                "token": token,
                "expires_at": expires_at,
                "permissions": {"contents": "read"},
                "repository_selection": "selected",
            })
            .to_string(),
        )
    }

    fn status_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn credential(server: &LoopbackServer, downscope: Option<Downscope>) -> GithubAppCredential {
        GithubAppCredential::with_api_base(
            "github-app-fixture".to_owned(),
            Installation {
                app_id: NonZeroU64::new(APP_ID).expect("non-zero"),
                installation_id: NonZeroU64::new(INSTALLATION_ID).expect("non-zero"),
                downscope,
            },
            GithubAppKey::from_pem(KEY_PEM).expect("the fixture key parses"),
            vec!["api.github.com".to_owned()],
            &server.url(),
        )
        .expect("the credential builds")
    }

    fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request
            .split("\r\n\r\n")
            .next()
            .expect("request head")
            .lines()
            .skip(1)
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name).then(|| value.trim())
            })
    }

    fn body(request: &str) -> &str {
        request.split_once("\r\n\r\n").map_or("", |(_, body)| body)
    }

    fn jwt(request: &str) -> &str {
        header(request, "authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .expect("the mint carries a bearer JWT")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn github_app_mints_once_per_cache_window() {
        let server = LoopbackServer::once(&token_response(TOKEN, time::Duration::hours(1)));
        let shared = Arc::new(credential(&server, None));

        let (first, second) = tokio::join!(
            tokio::spawn({
                let credential = Arc::clone(&shared);
                async move { credential.resolve().await }
            }),
            tokio::spawn({
                let credential = Arc::clone(&shared);
                async move { credential.resolve().await }
            }),
        );
        first.expect("join").expect("first resolution");
        second.expect("join").expect("second resolution");
        shared.resolve().await.expect("cached resolution");

        let request = server.request_text();
        assert!(
            request.starts_with(&format!(
                "POST /app/installations/{INSTALLATION_ID}/access_tokens HTTP/1.1\r\n"
            )),
            "{request}"
        );
        assert!(server.recorded().is_empty(), "the token was minted twice");
        server.join();

        let renewing = LoopbackServer::sequence([
            token_response(TOKEN, time::Duration::minutes(4)),
            token_response(TOKEN, time::Duration::hours(1)),
        ]);
        let credential = credential(&renewing, None);
        credential.resolve().await.expect("first mint");
        credential
            .resolve()
            .await
            .expect("a token inside its last five minutes is minted again");
        for _ in 0..2 {
            assert!(
                renewing
                    .request_text()
                    .starts_with(&format!("POST /app/installations/{INSTALLATION_ID}/"))
            );
        }
        renewing.join();
    }

    #[tokio::test]
    async fn github_app_sends_a_verifiable_rs256_jwt() {
        let server = LoopbackServer::once(&token_response(TOKEN, time::Duration::hours(1)));
        let before = OffsetDateTime::now_utc().unix_timestamp();
        credential(&server, None)
            .resolve()
            .await
            .expect("resolution");
        let after = OffsetDateTime::now_utc().unix_timestamp();
        let request = server.request_text();

        assert_eq!(
            header(&request, "accept"),
            Some("application/vnd.github+json")
        );
        let token = jwt(&request);
        let (signing_input, signature) = token.rsplit_once('.').expect("three JWT segments");
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, PUBLIC_KEY_DER)
            .verify(
                signing_input.as_bytes(),
                &URL_SAFE_NO_PAD
                    .decode(signature)
                    .expect("signature encoding"),
            )
            .expect("the fixture's public key verifies the JWT");
        let (header_segment, claims_segment) =
            signing_input.split_once('.').expect("header and claims");
        let header_json: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(header_segment)
                .expect("header encoding"),
        )
        .expect("header JSON");
        assert_eq!(
            header_json,
            serde_json::json!({"alg": "RS256", "typ": "JWT"})
        );
        let claims: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(claims_segment)
                .expect("claims encoding"),
        )
        .expect("claims JSON");
        assert_eq!(claims["iss"], APP_ID);
        let iat = claims["iat"].as_i64().expect("numeric iat");
        let exp = claims["exp"].as_i64().expect("numeric exp");
        assert!((before - 60..=after - 60).contains(&iat), "iat {iat}");
        assert!((before + 540..=after + 540).contains(&exp), "exp {exp}");
        server.join();
    }

    #[tokio::test]
    async fn github_app_downscope_body_is_sent_only_when_configured() {
        let server = LoopbackServer::once(&token_response(TOKEN, time::Duration::hours(1)));
        credential(&server, None)
            .resolve()
            .await
            .expect("resolution");
        let request = server.request_text();
        assert_eq!(body(&request), "");
        assert_eq!(header(&request, "content-type"), None);
        server.join();

        let server = LoopbackServer::once(&token_response(TOKEN, time::Duration::hours(1)));
        let downscope = Downscope {
            repositories: Some(vec!["dekopon".to_owned()]),
            permissions: Some([("contents".to_owned(), "read".to_owned())].into()),
        };
        credential(&server, Some(downscope))
            .resolve()
            .await
            .expect("resolution");
        let request = server.request_text();
        assert_eq!(header(&request, "content-type"), Some("application/json"));
        let sent: serde_json::Value = serde_json::from_str(body(&request)).expect("downscope JSON");
        assert_eq!(
            sent,
            serde_json::json!({"repositories": ["dekopon"], "permissions": {"contents": "read"}})
        );
        server.join();

        let server = LoopbackServer::once(&token_response(TOKEN, time::Duration::hours(1)));
        let downscope = Downscope {
            repositories: None,
            permissions: Some([("issues".to_owned(), "write".to_owned())].into()),
        };
        credential(&server, Some(downscope))
            .resolve()
            .await
            .expect("resolution");
        let sent: serde_json::Value =
            serde_json::from_str(body(&server.request_text())).expect("downscope JSON");
        assert_eq!(
            sent,
            serde_json::json!({"permissions": {"issues": "write"}})
        );
        server.join();
    }

    #[tokio::test]
    async fn github_app_mint_401_requires_reauthorization() {
        let server = LoopbackServer::once(&status_response(
            "401 Unauthorized",
            r#"{"message":"A JSON web token could not be decoded"}"#,
        ));
        let error = credential(&server, None)
            .resolve()
            .await
            .expect_err("a refused JWT fails");
        assert_eq!(error, CredentialRefreshError::ReauthorizationRequired);
        server.join();
    }

    #[tokio::test]
    async fn github_app_mint_failures_other_than_401_are_transient() {
        for (response, category) in [
            (
                status_response("503 Service Unavailable", "{}"),
                "token-endpoint-unavailable",
            ),
            (
                status_response("404 Not Found", r#"{"message":"Not Found"}"#),
                "token-endpoint-rejected",
            ),
            (
                status_response("201 Created", r#"{"token":"#),
                "token-endpoint-protocol",
            ),
            (
                status_response(
                    "201 Created",
                    &serde_json::json!({"token": TOKEN, "expires_at": "soon"}).to_string(),
                ),
                "token-endpoint-protocol",
            ),
        ] {
            let server = LoopbackServer::once(&response);
            let error = credential(&server, None)
                .resolve()
                .await
                .expect_err("the mint fails");
            assert_eq!(error, CredentialRefreshError::Unavailable { category });
            server.join();
        }
    }

    #[tokio::test]
    async fn github_app_binds_the_token_only_to_its_destinations() {
        let server = LoopbackServer::once(&token_response(TOKEN, time::Duration::hours(1)));
        let credential = credential(&server, None);
        let bound = credential.resolve().await.expect("resolution");

        assert_eq!(credential.destinations(), ["api.github.com"]);
        assert_eq!(bound.destinations(), ["api.github.com"]);
        assert!(bound.covers("api.github.com"));
        assert!(!bound.covers("uploads.github.com"));
        assert!(!bound.covers("chatgpt.com"));
        server.join();
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn github_app_secrets_never_reach_debug_or_errors() {
        let logs = Captured::default();
        let writer = logs.clone();
        let _subscriber = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .finish(),
        );
        let server = LoopbackServer::sequence([
            token_response(TOKEN, time::Duration::minutes(1)),
            status_response("401 Unauthorized", "{}"),
            status_response(
                "201 Created",
                &serde_json::json!({"token": TOKEN, "expires_at": 7}).to_string(),
            ),
        ]);
        let credential = credential(&server, None);
        let bound = credential.resolve().await.expect("resolution");
        let reauthorization = credential.resolve().await.expect_err("401");
        let malformed = credential.resolve().await.expect_err("malformed");
        let jwts = (0..3)
            .map(|_| jwt(&server.request_text()).to_owned())
            .collect::<Vec<_>>();
        server.join();

        let key_lines = std::str::from_utf8(KEY_PEM)
            .expect("PEM text")
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let rendered = [
            format!("{credential:?}"),
            format!("{bound:?}"),
            format!("{reauthorization:?} {reauthorization}"),
            format!("{malformed:?} {malformed}"),
            String::from_utf8(logs.0.lock().clone()).expect("UTF-8 logs"),
        ];
        assert!(
            rendered[4].contains("broker_github_app_credential_reauth_required"),
            "the capture saw no events: {}",
            rendered[4]
        );
        for text in &rendered {
            assert!(!text.contains(TOKEN), "the token leaked: {text}");
            for jwt in &jwts {
                assert!(!text.contains(jwt.as_str()), "a JWT leaked: {text}");
            }
            for line in &key_lines {
                assert!(!text.contains(line.as_str()), "key bytes leaked: {text}");
            }
        }
    }
}
