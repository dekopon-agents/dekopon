use std::{
    io::{self, Read as _},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dekopon_core::{
    FileHygieneError, FileTier, SecretBytes, SecretDrn, TaskOutcome, offload, read_trusted_file,
};
use dekopon_telemetry::egress::{Timeout, client_builder};
use reqwest::Url;
use ring::rand::{SecureRandom as _, SystemRandom};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use url::Host;

use crate::{
    github_app::{GithubAppKey, HARD_MAX_GITHUB_APP_KEY_BYTES},
    secrets::{REFRESH_TIMEOUT, trace_source_cause},
};

const MAX_TTL_SECONDS: u32 = 300;
const MAX_JWKS_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SignedJwtConfig {
    pub(crate) private_key: PathBuf,
    pub(crate) runner_secret: SecretDrn,
    jwks_uri: String,
    helper: HelperPin,
    iss: String,
    aud: String,
    ttl: u32,
    claims: PersonaClaims,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct HelperPin {
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonaClaims {
    sub: String,
    org: String,
    role: String,
    email: String,
}

#[derive(Debug, Error)]
pub enum SignedJwtConfigError {
    #[error("signedJwt ttl must be in 1..=300 seconds")]
    Ttl,
    #[error("signedJwt iss must be an absolute HTTPS URL")]
    Issuer,
    #[error("signedJwt aud must be one exact HTTPS origin")]
    Audience,
    #[error("signedJwt kid is derived from privateKey; remove the kid field")]
    KidRemoved,
    #[error("signedJwt jwksUri must be an absolute HTTPS URL or a loopback HTTP URL")]
    JwksUri,
    #[error("signedJwt JWKS HTTP client could not be built")]
    JwksClient,
    #[error("signedJwt helper needs an absolute path and a lowercase hex sha256 digest")]
    Helper,
    #[error("signedJwt claims must be nonempty")]
    Claims,
    #[error("signedJwt privateKey must be absolute")]
    KeyPath,
    #[error("signedJwt privateKey is not trusted input")]
    KeyRead(#[source] FileHygieneError),
    #[error("signedJwt privateKey could not be loaded: {reason}")]
    KeyFormat { reason: String },
    #[error(
        "signedJwt requires a dedicated runnerSecret with httpBearer bindings for the persona capabilities"
    )]
    RunnerBinding,
    #[error("signedJwt and vmExecStdin must be bound together for one POST without a query")]
    SinkBinding,
}

pub(crate) struct IssuedToken {
    pub(crate) token: SecretBytes,
    pub(crate) jti: String,
    pub(crate) exp: u64,
    pub(crate) helper: String,
}

#[derive(Debug, Error)]
pub(crate) enum IssueError {
    #[error("the system clock is before the Unix epoch")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("signedJwt expiry is outside the timestamp range")]
    Expiry,
    #[error("signedJwt could not generate a token identifier")]
    Random,
    #[error("signedJwt could not sign the token")]
    Signing,
    #[error("signedJwt helper does not match its pinned digest")]
    HelperMismatch,
    #[error("signedJwt helper could not be read ({kind:?})")]
    HelperRead { kind: io::ErrorKind },
    #[error("signedJwt helper read task {0}")]
    HelperTask(TaskOutcome),
    #[error("the JWKS could not be fetched")]
    JwksUnreachable,
    #[error("the JWKS response is not a key set")]
    JwksMalformed,
    #[error("the published JWKS does not serve the signing key's kid")]
    JwksKidNotServed,
}

impl IssueError {
    pub(crate) const fn category(&self) -> &'static str {
        match self {
            Self::Clock(_) | Self::Expiry => "clock",
            Self::Random => "random",
            Self::Signing => "signing",
            Self::HelperMismatch | Self::HelperRead { .. } | Self::HelperTask(_) => "helper-pin",
            Self::JwksUnreachable => "jwks-unreachable",
            Self::JwksMalformed => "jwks-malformed",
            Self::JwksKidNotServed => "jwks-kid-not-served",
        }
    }

    fn with_cause(self, cause: &(impl std::fmt::Debug + 'static)) -> Self {
        trace_source_cause(self.category(), cause);
        self
    }
}

pub(crate) struct SelfSignedIssuer {
    key: GithubAppKey,
    kid: String,
    n: String,
    e: String,
    client: reqwest::Client,
    config: SignedJwtConfig,
}

pub(crate) fn thumbprint(n: &str, e: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(format!(
        r#"{{"e":"{e}","kty":"RSA","n":"{n}"}}"#
    )))
}

#[derive(Deserialize)]
struct KeySet {
    keys: Vec<PublishedKey>,
}

#[derive(Deserialize)]
struct PublishedKey {
    kty: String,
    kid: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

impl SelfSignedIssuer {
    pub(crate) fn open(
        config: SignedJwtConfig,
        uid: u32,
        digest: &mut Sha256,
    ) -> Result<Self, Vec<SignedJwtConfigError>> {
        let mut errors = Vec::new();
        if !(1..=MAX_TTL_SECONDS).contains(&config.ttl) {
            errors.push(SignedJwtConfigError::Ttl);
        }
        if !Url::parse(&config.iss).is_ok_and(|url| url.scheme() == "https" && url.has_host()) {
            errors.push(SignedJwtConfigError::Issuer);
        }
        if !Url::parse(&config.aud).is_ok_and(|url| {
            url.scheme() == "https" && url.origin().ascii_serialization() == config.aud
        }) {
            errors.push(SignedJwtConfigError::Audience);
        }
        if !Url::parse(&config.jwks_uri).is_ok_and(|url| {
            url.scheme() == "https"
                || url.scheme() == "http"
                    && matches!(url.host(), Some(Host::Ipv4(ip)) if ip.is_loopback())
        }) {
            errors.push(SignedJwtConfigError::JwksUri);
        }
        if !config.helper.path.is_absolute()
            || config.helper.sha256.len() != 64
            || !config
                .helper
                .sha256
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            errors.push(SignedJwtConfigError::Helper);
        }
        if [
            &config.claims.sub,
            &config.claims.org,
            &config.claims.role,
            &config.claims.email,
        ]
        .iter()
        .any(|value| value.trim().is_empty())
        {
            errors.push(SignedJwtConfigError::Claims);
        }
        if !config.private_key.is_absolute() {
            errors.push(SignedJwtConfigError::KeyPath);
            return Err(errors);
        }
        let pem = match read_trusted_file(
            &config.private_key,
            uid,
            FileTier::Private,
            HARD_MAX_GITHUB_APP_KEY_BYTES,
        ) {
            Ok(pem) => pem,
            Err(error) => {
                errors.push(SignedJwtConfigError::KeyRead(error));
                return Err(errors);
            }
        };
        let key = match GithubAppKey::from_pem(&pem) {
            Ok(key) => key,
            Err(error) => {
                errors.push(SignedJwtConfigError::KeyFormat {
                    reason: error.to_string(),
                });
                return Err(errors);
            }
        };
        if !errors.is_empty() {
            return Err(errors);
        }
        let client = match client_builder(
            reqwest::Client::builder(),
            Timeout::Request(REFRESH_TIMEOUT),
            crate::EGRESS_USER_AGENT,
            [],
        )
        .build()
        {
            Ok(client) => client,
            Err(cause) => {
                trace_source_cause("configuration", &cause);
                return Err(vec![SignedJwtConfigError::JwksClient]);
            }
        };
        digest.update([2]);
        digest.update(pem.len().to_le_bytes());
        digest.update(&pem);
        let components = key.components();
        let n = URL_SAFE_NO_PAD.encode(&components.n);
        let e = URL_SAFE_NO_PAD.encode(&components.e);
        Ok(Self {
            key,
            kid: thumbprint(&n, &e),
            n,
            e,
            client,
            config,
        })
    }

    async fn confirm_published(&self) -> Result<(), IssueError> {
        let mut response = self
            .client
            .get(&self.config.jwks_uri)
            .send()
            .await
            .map_err(|cause| IssueError::JwksUnreachable.with_cause(&cause))?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            let error = IssueError::JwksUnreachable;
            tracing::debug!(
                category = error.category(),
                http.response.status_code = status.as_u16(),
                "signedJwt JWKS request refused"
            );
            return Err(error);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|cause| IssueError::JwksUnreachable.with_cause(&cause))?
        {
            if body.len() + chunk.len() > MAX_JWKS_BYTES {
                return Err(IssueError::JwksMalformed);
            }
            body.extend_from_slice(&chunk);
        }
        let set: KeySet = serde_json::from_slice(&body)
            .map_err(|cause| IssueError::JwksMalformed.with_cause(&cause))?;
        let served = set.keys.iter().any(|key| {
            key.kty == "RSA"
                && key.kid.as_deref() == Some(self.kid.as_str())
                && key.n.as_deref() == Some(self.n.as_str())
                && key.e.as_deref() == Some(self.e.as_str())
        });
        if served {
            Ok(())
        } else {
            Err(IssueError::JwksKidNotServed)
        }
    }

    async fn confirm_helper(&self) -> Result<(), IssueError> {
        let path = self.config.helper.path.clone();
        let digest = offload(move || {
            let mut file = std::fs::File::open(path)?;
            let mut hasher = Sha256::new();
            let mut chunk = [0; 8192];
            loop {
                match file.read(&mut chunk)? {
                    0 => break,
                    count => hasher.update(&chunk[..count]),
                }
            }
            Ok::<_, io::Error>(hasher.finalize())
        })
        .await
        .map_err(|cause| {
            let outcome = TaskOutcome::from(cause);
            let kind = outcome.category();
            let error = IssueError::HelperTask(outcome);
            tracing::debug!(
                category = error.category(),
                error.task = kind,
                "signedJwt helper read task failed"
            );
            error
        })?
        .map_err(|cause| IssueError::HelperRead { kind: cause.kind() }.with_cause(&cause))?;
        if digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            == self.config.helper.sha256
        {
            Ok(())
        } else {
            Err(IssueError::HelperMismatch)
        }
    }

    #[expect(
        clippy::map_err_ignore,
        reason = "ring errors have no diagnostic detail"
    )]
    pub(crate) async fn issue(&self) -> Result<IssuedToken, IssueError> {
        self.confirm_helper().await?;
        self.confirm_published().await?;
        let iat = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let exp = iat
            .checked_add(u64::from(self.config.ttl))
            .ok_or(IssueError::Expiry)?;
        let mut id = [0; 16];
        SystemRandom::new()
            .fill(&mut id)
            .map_err(|_| IssueError::Random)?;
        let jti = URL_SAFE_NO_PAD.encode(id);
        let header = URL_SAFE_NO_PAD
            .encode(serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": self.kid}).to_string());
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "iss": self.config.iss,
                "iat": iat,
                "exp": exp,
                "jti": jti,
                "aud": self.config.aud,
                "sub": self.config.claims.sub,
                "org": self.config.claims.org,
                "role": self.config.claims.role,
                "email": self.config.claims.email,
            })
            .to_string(),
        );
        let input = format!("{header}.{claims}");
        let signature = self
            .key
            .sign_rs256(input.as_bytes())
            .map_err(|_| IssueError::Signing)?;
        let token =
            SecretBytes::new(format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature)).into_bytes());
        Ok(IssuedToken {
            token,
            jti,
            exp,
            helper: self.config.helper.path.to_string_lossy().into_owned(),
        })
    }
}
