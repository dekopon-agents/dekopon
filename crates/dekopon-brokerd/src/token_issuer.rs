use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dekopon_core::{FileHygieneError, FileTier, SecretBytes, SecretDrn, read_trusted_file};
use reqwest::Url;
use ring::rand::{SecureRandom as _, SystemRandom};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::github_app::{GithubAppKey, HARD_MAX_GITHUB_APP_KEY_BYTES};

const ISSUER: &str = "https://id.xavierlange.com/dekopon";
const MAX_TTL_SECONDS: u32 = 300;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct SignedJwtConfig {
    pub(crate) private_key: PathBuf,
    pub(crate) runner_secret: SecretDrn,
    kid: String,
    aud: String,
    ttl: u32,
    claims: PersonaClaims,
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
    #[error("signedJwt aud must be one exact HTTPS origin")]
    Audience,
    #[error("signedJwt kid must be nonempty")]
    Kid,
    #[error("signedJwt claims must be nonempty")]
    Claims,
    #[error("signedJwt privateKey must be absolute")]
    KeyPath,
    #[error("signedJwt privateKey is not trusted input")]
    KeyRead(#[source] FileHygieneError),
    #[error("signedJwt privateKey could not be loaded: {reason}")]
    KeyFormat { reason: String },
    #[error("signedJwt requires a dedicated runnerSecret with a vm.exec httpBearer binding")]
    RunnerBinding,
    #[error("signedJwt and vmExecStdin must be bound together, exclusively on vm.exec")]
    SinkBinding,
}

pub(crate) trait TokenIssuer {
    fn issue(&self) -> Result<IssuedToken, IssueError>;
}

pub(crate) struct IssuedToken {
    pub(crate) token: SecretBytes,
    pub(crate) jti: String,
    pub(crate) exp: u64,
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
}

impl IssueError {
    pub(crate) const fn category(&self) -> &'static str {
        match self {
            Self::Clock(_) | Self::Expiry => "clock",
            Self::Random => "random",
            Self::Signing => "signing",
        }
    }
}

pub(crate) struct SelfSignedIssuer {
    key: GithubAppKey,
    config: SignedJwtConfig,
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
        if !Url::parse(&config.aud).is_ok_and(|url| {
            url.scheme() == "https" && url.origin().ascii_serialization() == config.aud
        }) {
            errors.push(SignedJwtConfigError::Audience);
        }
        if config.kid.trim().is_empty() {
            errors.push(SignedJwtConfigError::Kid);
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
        digest.update(pem.len().to_le_bytes());
        digest.update(&pem);
        Ok(Self { key, config })
    }
}

impl TokenIssuer for SelfSignedIssuer {
    #[expect(
        clippy::map_err_ignore,
        reason = "ring errors have no diagnostic detail"
    )]
    fn issue(&self) -> Result<IssuedToken, IssueError> {
        let iat = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let exp = iat
            .checked_add(u64::from(self.config.ttl))
            .ok_or(IssueError::Expiry)?;
        let mut id = [0; 16];
        SystemRandom::new()
            .fill(&mut id)
            .map_err(|_| IssueError::Random)?;
        let jti = URL_SAFE_NO_PAD.encode(id);
        let header = URL_SAFE_NO_PAD.encode(
            serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": self.config.kid}).to_string(),
        );
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "iss": ISSUER,
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
        Ok(IssuedToken { token, jti, exp })
    }
}
