//! Owner-only private secret map and broker-side source adapters.
//!
//! Public DRNs are deliberately absent from provider JSON and WIT. This module parses physical
//! locators at startup without contacting their backends, then resolves one already-authorized
//! snapshot per invocation. Bootstrap credentials are strict files and are never DRN-addressable.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_broker::{
    BrokerBuildError, MAX_SECRET_BINDINGS, SecretCatalog, SecretMaterial, SecretResolutionError,
    SecretResolver, SecretUseBinding,
};
use dekopon_capability::HttpPathRule;
use dekopon_core::{CapabilityId, Redacted, SecretDrn, SecretSinkKind};
use futures_util::StreamExt as _;
use hmac::{Hmac, KeyInit as _, Mac as _};
use reqwest::{Method, Url, header};
use serde::{
    Deserialize,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use time::{OffsetDateTime, format_description::BorrowedFormatItem, macros::format_description};
use tokio::{
    fs::OpenOptions,
    io::AsyncReadExt as _,
    time::{Instant, timeout_at},
};

pub const SECRET_MAP_API_VERSION: &str = "dekopon.dev/secret-map/v1alpha1";
pub const HARD_MAX_SECRET_MAP_BYTES: usize = 1024 * 1024;
pub const HARD_MAX_SECRETS: usize = 256;
pub const HARD_MAX_SECRET_BYTES: usize = dekopon_http_host::MAX_CREDENTIAL_BYTES;
pub const HARD_MAX_SOURCE_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_BOOTSTRAP_TOKEN_BYTES: usize = 16 * 1024;
const MAX_DOCUMENT_SCALAR_BYTES: usize = 64 * 1024;
const MAX_SOURCE_RESPONSE_HEADERS: usize = 128;
const MAX_SOURCE_RESPONSE_HEADER_BYTES: usize = 64 * 1024;
const MAX_BINDINGS_PER_SECRET: usize = 64;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
enum SecretMapApiVersion {
    #[serde(rename = "dekopon.dev/secret-map/v1alpha1")]
    V1Alpha1,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SecretMapFile {
    #[allow(
        dead_code,
        reason = "strict enum deserialization validates the version"
    )]
    api_version: SecretMapApiVersion,
    map_revision: String,
    secrets: Vec<SecretEntry>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SecretEntry {
    drn: SecretDrn,
    source: SecretSource,
    #[serde(default)]
    projection: Projection,
    bindings: Vec<Binding>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(
    tag = "kind",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum SecretSource {
    SecureFile {
        path: PathBuf,
    },
    KubernetesProjection {
        root: PathBuf,
        key: String,
        declared_origin: KubernetesProjectionOrigin,
        #[serde(default)]
        acknowledge_non_secret_source: bool,
    },
    OnePasswordConnect {
        endpoint: String,
        token_file: PathBuf,
        vault: String,
        item: String,
        field: String,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    VaultKv1 {
        endpoint: String,
        token_file: PathBuf,
        #[serde(default)]
        namespace: Option<String>,
        mount: String,
        path: String,
        key: String,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    VaultKv2 {
        endpoint: String,
        token_file: PathBuf,
        #[serde(default)]
        namespace: Option<String>,
        mount: String,
        path: String,
        key: String,
        #[serde(default)]
        version: Option<u64>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    AwsSecretsManager {
        #[serde(default)]
        endpoint: Option<String>,
        region: String,
        credentials_file: PathBuf,
        secret_id: String,
        #[serde(default)]
        version_id: Option<String>,
        #[serde(default)]
        version_stage: Option<String>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    AwsSsmParameter {
        #[serde(default)]
        endpoint: Option<String>,
        region: String,
        credentials_file: PathBuf,
        name: String,
        #[serde(default)]
        selector: Option<String>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    GcpSecretManager {
        #[serde(default)]
        endpoint: Option<String>,
        token_file: PathBuf,
        project: String,
        #[serde(default)]
        location: Option<String>,
        secret: String,
        version: String,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    AzureKeyVault {
        vault_url: String,
        token_file: PathBuf,
        secret: String,
        #[serde(default)]
        version: Option<String>,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    KubernetesApi {
        endpoint: String,
        token_file: PathBuf,
        namespace: String,
        object_kind: KubernetesObjectKind,
        name: String,
        key: String,
        #[serde(default)]
        acknowledge_non_secret_source: bool,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum KubernetesObjectKind {
    Secret,
    ConfigMap,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum KubernetesProjectionOrigin {
    Secret,
    ConfigMap,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Projection {
    #[serde(default)]
    format: DocumentFormat,
    #[serde(default)]
    pointer: Option<String>,
    #[serde(default)]
    decode_base64: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
enum DocumentFormat {
    #[default]
    Raw,
    Utf8,
    Json,
    Yaml,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Binding {
    id: String,
    capability: CapabilityId,
    sink: SecretSinkKind,
    #[serde(default)]
    basic_username: Option<String>,
    allowed_hosts: Vec<String>,
    allowed_methods: Vec<String>,
    allowed_paths: Vec<HttpPathRule>,
    #[serde(default)]
    allow_query: bool,
    #[serde(default = "default_max_injections")]
    max_injections: u32,
}

const fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

const fn default_max_injections() -> u32 {
    1
}

#[derive(Clone)]
struct ResolvedEntry {
    source: SecretSource,
    projection: Projection,
}

struct MapResolver {
    entries: BTreeMap<SecretDrn, ResolvedEntry>,
    client: reqwest::Client,
    expected_uid: u32,
}

impl fmt::Debug for MapResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MapResolver")
            .field("entries", &self.entries.len())
            .field("sources", &"[BROKER-PRIVATE]")
            .finish()
    }
}

#[async_trait]
impl SecretResolver for MapResolver {
    async fn resolve(&self, secret: &SecretDrn) -> Result<SecretMaterial, SecretResolutionError> {
        let Some(entry) = self.entries.get(secret) else {
            return Err(SecretResolutionError {
                category: "missing",
            });
        };
        let bytes = self.resolve_source(&entry.source).await.map_err(|error| {
            tracing::warn!(
                event = "secret_source_resolution_failed",
                source_kind = entry.source.kind(),
                category = error.category(),
            );
            SecretResolutionError {
                category: error.category(),
            }
        })?;
        project(bytes, &entry.projection)
            .map(SecretMaterial::new)
            .map_err(|error| {
                tracing::warn!(
                    event = "secret_projection_failed",
                    source_kind = entry.source.kind(),
                    category = error.category(),
                );
                SecretResolutionError {
                    category: error.category(),
                }
            })
    }
}

impl MapResolver {
    async fn resolve_source(&self, source: &SecretSource) -> Result<Vec<u8>, SourceError> {
        match source {
            SecretSource::SecureFile { path } => {
                read_private_file(path, self.expected_uid, HARD_MAX_SOURCE_RESPONSE_BYTES).await
            }
            SecretSource::KubernetesProjection { root, key, .. } => {
                read_kubernetes_projection(root, key).await
            }
            SecretSource::OnePasswordConnect {
                endpoint,
                token_file,
                vault,
                item,
                field,
                timeout_ms,
            } => {
                let token = read_token(token_file, self.expected_uid).await?;
                let mut url = endpoint_url(endpoint, true)?;
                {
                    let mut segments = url
                        .path_segments_mut()
                        .map_err(|source| classified(SourceError::Config, &source))?;
                    segments.extend(["v1", "vaults", vault, "items", item]);
                }
                let value = self
                    .request_json(
                        Method::GET,
                        url,
                        Some((header::AUTHORIZATION, format!("Bearer {}", token.expose()))),
                        None,
                        *timeout_ms,
                    )
                    .await?;
                let fields = value
                    .get("fields")
                    .and_then(Value::as_array)
                    .ok_or(SourceError::Malformed)?;
                let mut matches = fields.iter().filter(|candidate| {
                    candidate.get("id").and_then(Value::as_str) == Some(field)
                        || candidate.get("label").and_then(Value::as_str) == Some(field)
                });
                let selected = matches.next().ok_or(SourceError::Missing)?;
                if matches.next().is_some() {
                    return Err(SourceError::Malformed);
                }
                reject_bootstrap_reflection(
                    scalar_bytes(selected.get("value").ok_or(SourceError::Missing)?)?,
                    &[token.expose()],
                )
            }
            SecretSource::VaultKv1 {
                endpoint,
                token_file,
                namespace,
                mount,
                path,
                key,
                timeout_ms,
            } => {
                self.vault_read(
                    endpoint,
                    token_file,
                    namespace.as_deref(),
                    mount,
                    path,
                    false,
                    None,
                    key,
                    *timeout_ms,
                )
                .await
            }
            SecretSource::VaultKv2 {
                endpoint,
                token_file,
                namespace,
                mount,
                path,
                key,
                version,
                timeout_ms,
            } => {
                self.vault_read(
                    endpoint,
                    token_file,
                    namespace.as_deref(),
                    mount,
                    path,
                    true,
                    *version,
                    key,
                    *timeout_ms,
                )
                .await
            }
            SecretSource::AwsSecretsManager {
                endpoint,
                region,
                credentials_file,
                secret_id,
                version_id,
                version_stage,
                timeout_ms,
            } => {
                let endpoint = endpoint
                    .clone()
                    .unwrap_or_else(|| format!("https://secretsmanager.{region}.amazonaws.com"));
                let mut body = serde_json::Map::from_iter([(
                    "SecretId".to_owned(),
                    Value::String(secret_id.clone()),
                )]);
                if let Some(value) = version_id {
                    body.insert("VersionId".to_owned(), Value::String(value.clone()));
                }
                if let Some(value) = version_stage {
                    body.insert("VersionStage".to_owned(), Value::String(value.clone()));
                }
                let (value, credentials) = self
                    .aws_post(
                        &endpoint,
                        region,
                        "secretsmanager",
                        "secretsmanager.GetSecretValue",
                        credentials_file,
                        Value::Object(body),
                        *timeout_ms,
                    )
                    .await?;
                let material = if let Some(text) = value.get("SecretString") {
                    scalar_bytes(text)?
                } else {
                    let encoded = value
                        .get("SecretBinary")
                        .and_then(Value::as_str)
                        .ok_or(SourceError::Missing)?;
                    STANDARD
                        .decode(encoded)
                        .map_err(|source| classified(SourceError::Malformed, &source))?
                };
                reject_bootstrap_reflection(
                    material,
                    &[
                        credentials.secret_access_key.expose(),
                        credentials
                            .session_token
                            .as_ref()
                            .map_or("", |token| token.expose()),
                    ],
                )
            }
            SecretSource::AwsSsmParameter {
                endpoint,
                region,
                credentials_file,
                name,
                selector,
                timeout_ms,
            } => {
                let endpoint = endpoint
                    .clone()
                    .unwrap_or_else(|| format!("https://ssm.{region}.amazonaws.com"));
                let selected = selector
                    .as_ref()
                    .map_or_else(|| name.clone(), |selector| format!("{name}:{selector}"));
                let (value, credentials) = self
                    .aws_post(
                        &endpoint,
                        region,
                        "ssm",
                        "AmazonSSM.GetParameter",
                        credentials_file,
                        serde_json::json!({"Name": selected, "WithDecryption": true}),
                        *timeout_ms,
                    )
                    .await?;
                reject_bootstrap_reflection(
                    scalar_bytes(
                        value
                            .pointer("/Parameter/Value")
                            .ok_or(SourceError::Missing)?,
                    )?,
                    &[
                        credentials.secret_access_key.expose(),
                        credentials
                            .session_token
                            .as_ref()
                            .map_or("", |token| token.expose()),
                    ],
                )
            }
            SecretSource::GcpSecretManager {
                endpoint,
                token_file,
                project,
                location,
                secret,
                version,
                timeout_ms,
            } => {
                let token = read_token(token_file, self.expected_uid).await?;
                let base = endpoint
                    .as_deref()
                    .unwrap_or("https://secretmanager.googleapis.com");
                let mut url = endpoint_url(base, true)?;
                {
                    let mut segments = url
                        .path_segments_mut()
                        .map_err(|source| classified(SourceError::Config, &source))?;
                    segments.push("v1").push("projects").push(project);
                    if let Some(location) = location {
                        segments.push("locations").push(location);
                    }
                    segments
                        .push("secrets")
                        .push(secret)
                        .push("versions")
                        .push(&format!("{version}:access"));
                }
                let value = self
                    .request_json(
                        Method::GET,
                        url,
                        Some((header::AUTHORIZATION, format!("Bearer {}", token.expose()))),
                        None,
                        *timeout_ms,
                    )
                    .await?;
                let encoded = value
                    .pointer("/payload/data")
                    .and_then(Value::as_str)
                    .ok_or(SourceError::Missing)?;
                let material = STANDARD
                    .decode(encoded)
                    .map_err(|source| classified(SourceError::Malformed, &source))?;
                let expected_crc = value
                    .pointer("/payload/dataCrc32c")
                    .and_then(|value| {
                        value
                            .as_str()
                            .and_then(|value| value.parse::<u32>().ok())
                            .or_else(|| value.as_u64().and_then(|value| u32::try_from(value).ok()))
                    })
                    .ok_or(SourceError::Malformed)?;
                if crc32c(&material) != expected_crc {
                    return Err(SourceError::Integrity);
                }
                reject_bootstrap_reflection(material, &[token.expose()])
            }
            SecretSource::AzureKeyVault {
                vault_url,
                token_file,
                secret,
                version,
                timeout_ms,
            } => {
                let token = read_token(token_file, self.expected_uid).await?;
                let mut url = endpoint_url(vault_url, true)?;
                {
                    let mut segments = url
                        .path_segments_mut()
                        .map_err(|source| classified(SourceError::Config, &source))?;
                    segments.push("secrets").push(secret);
                    if let Some(version) = version {
                        segments.push(version);
                    }
                }
                url.query_pairs_mut().append_pair("api-version", "7.4");
                let value = self
                    .request_json(
                        Method::GET,
                        url,
                        Some((header::AUTHORIZATION, format!("Bearer {}", token.expose()))),
                        None,
                        *timeout_ms,
                    )
                    .await?;
                reject_bootstrap_reflection(
                    scalar_bytes(value.get("value").ok_or(SourceError::Missing)?)?,
                    &[token.expose()],
                )
            }
            SecretSource::KubernetesApi {
                endpoint,
                token_file,
                namespace,
                object_kind,
                name,
                key,
                timeout_ms,
                ..
            } => {
                let token = read_token(token_file, self.expected_uid).await?;
                let mut url = endpoint_url(endpoint, true)?;
                let collection = match object_kind {
                    KubernetesObjectKind::Secret => "secrets",
                    KubernetesObjectKind::ConfigMap => "configmaps",
                };
                {
                    let mut segments = url
                        .path_segments_mut()
                        .map_err(|source| classified(SourceError::Config, &source))?;
                    segments.extend(["api", "v1", "namespaces", namespace, collection, name]);
                }
                let value = self
                    .request_json(
                        Method::GET,
                        url,
                        Some((header::AUTHORIZATION, format!("Bearer {}", token.expose()))),
                        None,
                        *timeout_ms,
                    )
                    .await?;
                let material = match object_kind {
                    KubernetesObjectKind::Secret => {
                        let encoded = value
                            .pointer(&format!("/data/{}", escape_pointer(key)))
                            .and_then(Value::as_str)
                            .ok_or(SourceError::Missing)?;
                        STANDARD
                            .decode(encoded)
                            .map_err(|source| classified(SourceError::Malformed, &source))?
                    }
                    KubernetesObjectKind::ConfigMap => {
                        if let Some(text) = value
                            .pointer(&format!("/data/{}", escape_pointer(key)))
                            .and_then(Value::as_str)
                        {
                            text.as_bytes().to_vec()
                        } else {
                            let encoded = value
                                .pointer(&format!("/binaryData/{}", escape_pointer(key)))
                                .and_then(Value::as_str)
                                .ok_or(SourceError::Missing)?;
                            STANDARD
                                .decode(encoded)
                                .map_err(|source| classified(SourceError::Malformed, &source))?
                        }
                    }
                };
                reject_bootstrap_reflection(material, &[token.expose()])
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn vault_read(
        &self,
        endpoint: &str,
        token_file: &Path,
        namespace: Option<&str>,
        mount: &str,
        path: &str,
        kv2: bool,
        version: Option<u64>,
        key: &str,
        timeout_ms: u64,
    ) -> Result<Vec<u8>, SourceError> {
        let token = read_token(token_file, self.expected_uid).await?;
        let mut url = endpoint_url(endpoint, true)?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|source| classified(SourceError::Config, &source))?;
            segments.push("v1").push(mount);
            if kv2 {
                segments.push("data");
            }
            for part in
                logical_parts(path).map_err(|source| classified(SourceError::Config, &source))?
            {
                segments.push(part);
            }
        }
        if let Some(version) = version {
            url.query_pairs_mut()
                .append_pair("version", &version.to_string());
        }
        let mut request = self.client.get(url).header("x-vault-token", token.expose());
        if let Some(namespace) = namespace {
            request = request.header("x-vault-namespace", namespace);
        }
        let value = bounded_json(request, timeout_ms).await?;
        let data = if kv2 {
            value.pointer("/data/data")
        } else {
            value.get("data")
        }
        .and_then(Value::as_object)
        .ok_or(SourceError::Malformed)?;
        reject_bootstrap_reflection(
            scalar_bytes(data.get(key).ok_or(SourceError::Missing)?)?,
            &[token.expose()],
        )
    }

    async fn request_json(
        &self,
        method: Method,
        url: Url,
        authorization: Option<(header::HeaderName, String)>,
        body: Option<Vec<u8>>,
        timeout_ms: u64,
    ) -> Result<Value, SourceError> {
        let mut request = self.client.request(method, url);
        if let Some((name, value)) = authorization {
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request
                .header(header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        bounded_json(request, timeout_ms).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn aws_post(
        &self,
        endpoint: &str,
        region: &str,
        service: &str,
        target: &str,
        credentials_file: &Path,
        body: Value,
        timeout_ms: u64,
    ) -> Result<(Value, AwsCredentialsFile), SourceError> {
        let credentials = read_aws_credentials(credentials_file, self.expected_uid).await?;
        let url = endpoint_url(endpoint, true)?;
        let host = url.host_str().ok_or(SourceError::Config)?;
        let authority = url
            .port()
            .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
        let body = serde_json::to_vec(&body)
            .map_err(|source| classified(SourceError::Malformed, &source))?;
        let now = OffsetDateTime::now_utc();
        const AMZ: &[BorrowedFormatItem<'_>] =
            format_description!("[year][month][day]T[hour][minute][second]Z");
        const DAY: &[BorrowedFormatItem<'_>] = format_description!("[year][month][day]");
        let amz_date = now
            .format(AMZ)
            .map_err(|source| classified(SourceError::Internal, &source))?;
        let date = now
            .format(DAY)
            .map_err(|source| classified(SourceError::Internal, &source))?;
        let payload_hash = hex_sha256(&body);
        let mut canonical_headers = format!(
            "content-type:application/x-amz-json-1.1\nhost:{authority}\nx-amz-date:{amz_date}\n"
        );
        let mut signed_headers = "content-type;host;x-amz-date".to_owned();
        if let Some(token) = &credentials.session_token {
            canonical_headers.push_str(&format!("x-amz-security-token:{}\n", token.expose()));
            signed_headers.push_str(";x-amz-security-token");
        }
        canonical_headers.push_str(&format!("x-amz-target:{target}\n"));
        signed_headers.push_str(";x-amz-target");
        let canonical_request = format!(
            "POST\n{}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            if url.path().is_empty() {
                "/"
            } else {
                url.path()
            }
        );
        let scope = format!("{date}/{region}/{service}/aws4_request");
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex_sha256(canonical_request.as_bytes())
        );
        let date_key = hmac(
            format!("AWS4{}", credentials.secret_access_key.expose()).as_bytes(),
            date.as_bytes(),
        )?;
        let region_key = hmac(&date_key, region.as_bytes())?;
        let service_key = hmac(&region_key, service.as_bytes())?;
        let signing_key = hmac(&service_key, b"aws4_request")?;
        let signature = hex(&hmac(&signing_key, string_to_sign.as_bytes())?);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        );
        let mut request = self
            .client
            .post(url)
            .header(header::CONTENT_TYPE, "application/x-amz-json-1.1")
            .header(header::HOST, authority)
            .header("x-amz-date", amz_date)
            .header("x-amz-target", target)
            .header(header::AUTHORIZATION, authorization)
            .body(body);
        if let Some(token) = &credentials.session_token {
            request = request.header("x-amz-security-token", token.expose());
        }
        let value = bounded_json(request, timeout_ms).await?;
        Ok((value, credentials))
    }
}

impl SecretSource {
    fn bootstrap_path(&self) -> Option<&Path> {
        match self {
            Self::OnePasswordConnect { token_file, .. }
            | Self::VaultKv1 { token_file, .. }
            | Self::VaultKv2 { token_file, .. }
            | Self::GcpSecretManager { token_file, .. }
            | Self::AzureKeyVault { token_file, .. }
            | Self::KubernetesApi { token_file, .. } => Some(token_file),
            Self::AwsSecretsManager {
                credentials_file, ..
            }
            | Self::AwsSsmParameter {
                credentials_file, ..
            } => Some(credentials_file),
            Self::SecureFile { .. } | Self::KubernetesProjection { .. } => None,
        }
    }

    fn material_file(&self) -> Option<PathBuf> {
        match self {
            Self::SecureFile { path } => Some(path.clone()),
            Self::KubernetesProjection { root, key, .. } => Some(root.join(key)),
            _ => None,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::SecureFile { .. } => "secure-file",
            Self::KubernetesProjection { .. } => "kubernetes-projection",
            Self::OnePasswordConnect { .. } => "one-password-connect",
            Self::VaultKv1 { .. } => "vault-kv1",
            Self::VaultKv2 { .. } => "vault-kv2",
            Self::AwsSecretsManager { .. } => "aws-secrets-manager",
            Self::AwsSsmParameter { .. } => "aws-ssm-parameter",
            Self::GcpSecretManager { .. } => "gcp-secret-manager",
            Self::AzureKeyVault { .. } => "azure-key-vault",
            Self::KubernetesApi { .. } => "kubernetes-api",
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::SecureFile { path } => validate_absolute(path),
            Self::KubernetesProjection {
                root,
                key,
                declared_origin,
                acknowledge_non_secret_source,
            } => {
                validate_absolute(root)?;
                validate_one_component(key)?;
                if matches!(declared_origin, KubernetesProjectionOrigin::ConfigMap)
                    && !acknowledge_non_secret_source
                {
                    return Err("ConfigMap projection requires acknowledgeNonSecretSource: true");
                }
                Ok(())
            }
            Self::OnePasswordConnect {
                endpoint,
                token_file,
                vault,
                item,
                field,
                timeout_ms,
            } => {
                validate_endpoint(endpoint, true)?;
                validate_absolute(token_file)?;
                validate_texts([vault, item, field])?;
                validate_timeout(*timeout_ms)
            }
            Self::VaultKv1 {
                endpoint,
                token_file,
                mount,
                path,
                key,
                timeout_ms,
                ..
            }
            | Self::VaultKv2 {
                endpoint,
                token_file,
                mount,
                path,
                key,
                timeout_ms,
                ..
            } => {
                validate_endpoint(endpoint, true)?;
                validate_absolute(token_file)?;
                validate_texts([mount, path, key])?;
                validate_locator_segment(mount)?;
                logical_parts(path)?;
                if let Self::VaultKv2 {
                    version: Some(0), ..
                } = self
                {
                    return Err("Vault KV v2 version must be greater than zero");
                }
                if let Self::VaultKv1 { namespace, .. } | Self::VaultKv2 { namespace, .. } = self {
                    validate_optional_text(namespace.as_ref())?;
                }
                validate_timeout(*timeout_ms)
            }
            Self::AwsSecretsManager {
                endpoint,
                region,
                credentials_file,
                secret_id,
                version_id,
                version_stage,
                timeout_ms,
            } => {
                if version_id.is_some() && version_stage.is_some() {
                    return Err("AWS versionId and versionStage are mutually exclusive");
                }
                if let Some(endpoint) = endpoint {
                    validate_endpoint(endpoint, true)?;
                }
                validate_absolute(credentials_file)?;
                validate_texts([region, secret_id])?;
                validate_locator_segment(region)?;
                validate_optional_text(version_id.as_ref())?;
                validate_optional_text(version_stage.as_ref())?;
                validate_timeout(*timeout_ms)
            }
            Self::AwsSsmParameter {
                endpoint,
                region,
                credentials_file,
                name,
                timeout_ms,
                ..
            } => {
                if let Some(endpoint) = endpoint {
                    validate_endpoint(endpoint, true)?;
                }
                validate_absolute(credentials_file)?;
                validate_texts([region, name])?;
                validate_locator_segment(region)?;
                if let Self::AwsSsmParameter { selector, .. } = self {
                    validate_optional_text(selector.as_ref())?;
                }
                validate_timeout(*timeout_ms)
            }
            Self::GcpSecretManager {
                endpoint,
                token_file,
                project,
                location,
                secret,
                version,
                timeout_ms,
            } => {
                if location.is_some() && endpoint.is_none() {
                    return Err("regional GCP secrets require an explicit regional endpoint");
                }
                if let Some(endpoint) = endpoint {
                    validate_endpoint(endpoint, true)?;
                }
                validate_absolute(token_file)?;
                validate_texts([project, secret, version])?;
                validate_locator_segment(project)?;
                validate_locator_segment(secret)?;
                validate_locator_segment(version)?;
                validate_optional_text(location.as_ref())?;
                if let Some(location) = location {
                    validate_locator_segment(location)?;
                }
                validate_timeout(*timeout_ms)
            }
            Self::AzureKeyVault {
                vault_url,
                token_file,
                secret,
                timeout_ms,
                ..
            } => {
                validate_endpoint(vault_url, true)?;
                validate_absolute(token_file)?;
                validate_texts([secret])?;
                validate_locator_segment(secret)?;
                if let Self::AzureKeyVault { version, .. } = self {
                    validate_optional_text(version.as_ref())?;
                    if let Some(version) = version {
                        validate_locator_segment(version)?;
                    }
                }
                validate_timeout(*timeout_ms)
            }
            Self::KubernetesApi {
                endpoint,
                token_file,
                namespace,
                object_kind,
                name,
                key,
                acknowledge_non_secret_source,
                timeout_ms,
            } => {
                validate_endpoint(endpoint, true)?;
                validate_absolute(token_file)?;
                validate_texts([namespace, name, key])?;
                for segment in [namespace, name, key] {
                    validate_locator_segment(segment)?;
                }
                if matches!(object_kind, KubernetesObjectKind::ConfigMap)
                    && !acknowledge_non_secret_source
                {
                    return Err("ConfigMap source requires acknowledgeNonSecretSource: true");
                }
                validate_timeout(*timeout_ms)
            }
        }
    }
}

/// Parses a strict owner-only map and constructs an executable broker catalog without network I/O.
pub async fn load(path: &Path, expected_uid: u32) -> Result<SecretCatalog, SecretMapError> {
    let bytes = read_private_file(path, expected_uid, HARD_MAX_SECRET_MAP_BYTES)
        .await
        .map_err(|source| SecretMapError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    let file = serde_yaml::from_slice::<SecretMapFile>(&bytes)
        .map_err(|source| SecretMapError::Decode { source })?;
    validate_map(file, path, expected_uid)
}

fn validate_map(
    file: SecretMapFile,
    map_path: &Path,
    expected_uid: u32,
) -> Result<SecretCatalog, SecretMapError> {
    let mut problems = Vec::new();
    if file.map_revision.is_empty()
        || file.map_revision.len() > 128
        || file.map_revision.trim() != file.map_revision
        || file
            .map_revision
            .bytes()
            .any(|byte| byte.is_ascii_control())
    {
        problems.push("mapRevision must be nonempty and at most 128 bytes".to_owned());
    }
    if file.secrets.is_empty() || file.secrets.len() > HARD_MAX_SECRETS {
        problems.push(format!(
            "secrets must contain between one and {HARD_MAX_SECRETS} entries"
        ));
    }
    let mut seen = BTreeSet::new();
    let mut bootstrap_paths = BTreeSet::new();
    let mut material_paths = BTreeSet::new();
    let mut binding_ids = BTreeSet::new();
    let mut binding_keys = BTreeSet::new();
    let mut entries = BTreeMap::new();
    let mut bindings = Vec::new();
    for entry in file.secrets {
        if !seen.insert(entry.drn.clone()) {
            problems.push(format!("duplicate DRN {}", entry.drn));
        }
        if let Err(reason) = entry.source.validate() {
            problems.push(format!("{} source is invalid: {reason}", entry.drn));
        }
        if let Some(path) = entry.source.bootstrap_path() {
            bootstrap_paths.insert(path.to_path_buf());
        }
        if let Some(path) = entry.source.material_file() {
            material_paths.insert(path);
        }
        if entry.bindings.is_empty() || entry.bindings.len() > MAX_BINDINGS_PER_SECRET {
            problems.push(format!("{} has an invalid binding count", entry.drn));
        }
        if matches!(
            entry.projection.format,
            DocumentFormat::Raw | DocumentFormat::Utf8
        ) && entry.projection.pointer.is_some()
        {
            problems.push(format!(
                "{} uses a pointer with a non-document projection",
                entry.drn
            ));
        }
        if entry
            .projection
            .pointer
            .as_deref()
            .is_some_and(|pointer| !valid_json_pointer(pointer))
        {
            problems.push(format!("{} uses an invalid RFC 6901 pointer", entry.drn));
        }
        for binding in entry.bindings {
            let resolved = SecretUseBinding {
                binding_id: binding.id,
                secret: entry.drn.clone(),
                capability: binding.capability,
                sink: binding.sink,
                basic_username: binding.basic_username,
                allowed_hosts: binding.allowed_hosts,
                allowed_methods: binding.allowed_methods,
                allowed_paths: binding.allowed_paths,
                allow_query: binding.allow_query,
                max_injections: binding.max_injections,
            };
            if !binding_ids.insert(resolved.binding_id.clone()) {
                problems.push(format!(
                    "duplicate secret binding identifier {:?}",
                    resolved.binding_id
                ));
            }
            if !binding_keys.insert((
                resolved.secret.clone(),
                resolved.capability.clone(),
                resolved.sink,
                resolved.basic_username.clone(),
            )) {
                problems.push(format!(
                    "{} has conflicting bindings for capability {}",
                    resolved.secret, resolved.capability
                ));
            }
            if let Err(error) = resolved.validate() {
                problems.push(format!(
                    "secret binding {:?} is invalid: {error}",
                    resolved.binding_id
                ));
            }
            bindings.push(resolved);
        }
        entries.insert(
            entry.drn,
            ResolvedEntry {
                source: entry.source,
                projection: entry.projection,
            },
        );
    }
    if bootstrap_paths.contains(map_path) {
        problems
            .push("private secret map cannot be used as a backend bootstrap credential".to_owned());
    }
    for path in material_paths {
        if path == map_path {
            problems.push("private secret map cannot be used as secret material".to_owned());
        }
        if bootstrap_paths.contains(&path) {
            problems.push(format!(
                "secret material path {} is also a backend bootstrap credential",
                path.display()
            ));
        }
    }
    if bindings.len() > MAX_SECRET_BINDINGS {
        problems.push(format!(
            "secret bindings contain {} entries; maximum is {MAX_SECRET_BINDINGS}",
            bindings.len()
        ));
    }
    if !problems.is_empty() {
        return Err(SecretMapError::Validation { problems });
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|source| SecretMapError::Client { source })?;
    let resolver = Arc::new(MapResolver {
        entries,
        client,
        expected_uid,
    });
    SecretCatalog::new(bindings, resolver)
        .and_then(|catalog| catalog.with_authority_revision(file.map_revision))
        .map_err(|source| SecretMapError::Catalog { source })
}

fn validate_timeout(value: u64) -> Result<(), &'static str> {
    (value > 0 && value <= 120_000)
        .then_some(())
        .ok_or("timeout must be between 1 and 120000 milliseconds")
}

fn validate_absolute(path: &Path) -> Result<(), &'static str> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err("path must be canonical absolute with no dot segments");
    }
    Ok(())
}

fn validate_one_component(value: &str) -> Result<(), &'static str> {
    let path = Path::new(value);
    if value.is_empty()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err("value must be one relative path component");
    }
    Ok(())
}

fn validate_locator_segment(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.contains(['/', '\\', '?', '#']) || matches!(value, "." | "..") {
        return Err("locator part must be one non-dot path segment");
    }
    Ok(())
}

fn validate_optional_text(value: Option<&String>) -> Result<(), &'static str> {
    match value {
        Some(value) => validate_texts([value]),
        None => Ok(()),
    }
}

fn validate_texts<'a>(values: impl IntoIterator<Item = &'a String>) -> Result<(), &'static str> {
    if values.into_iter().any(|value| {
        value.is_empty()
            || value.len() > 4096
            || value.trim() != value
            || value.bytes().any(|byte| byte.is_ascii_control())
    }) {
        return Err("locator parts must be nonempty bounded text without controls");
    }
    Ok(())
}

fn validate_endpoint(endpoint: &str, allow_loopback_http: bool) -> Result<(), &'static str> {
    let url = Url::parse(endpoint).map_err(|source| {
        tracing::debug!(
            event = "secret_source_configuration_cause",
            category = "invalid-endpoint",
            reason = %source,
        );
        "endpoint is not an absolute URL"
    })?;
    let host = url.host_str().ok_or("endpoint has no host")?;
    let loopback = host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    if url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (allow_loopback_http && url.scheme() == "http" && loopback))
    {
        return Err("endpoint must be credential-free HTTPS or explicit literal loopback HTTP");
    }
    Ok(())
}

fn endpoint_url(endpoint: &str, allow_loopback_http: bool) -> Result<Url, SourceError> {
    validate_endpoint(endpoint, allow_loopback_http)
        .map_err(|source| classified(SourceError::Config, &source))?;
    Url::parse(endpoint).map_err(|source| classified(SourceError::Config, &source))
}

fn logical_parts(path: &str) -> Result<Vec<&str>, &'static str> {
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.is_empty()
        || parts.iter().any(|part| {
            part.is_empty() || matches!(*part, "." | "..") || part.contains(['\\', '?', '#'])
        })
    {
        return Err("logical path is not canonical");
    }
    Ok(parts)
}

async fn read_token(path: &Path, uid: u32) -> Result<Redacted<String>, SourceError> {
    let bytes = read_private_file(path, uid, MAX_BOOTSTRAP_TOKEN_BYTES).await?;
    let text =
        String::from_utf8(bytes).map_err(|source| classified(SourceError::Malformed, &source))?;
    let text = text.trim_end_matches(['\r', '\n']);
    if text.is_empty()
        || text
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(SourceError::Malformed);
    }
    Ok(Redacted::new(text.to_owned()))
}

async fn read_private_file(path: &Path, uid: u32, maximum: usize) -> Result<Vec<u8>, SourceError> {
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .await
        .map_err(|source| classified(SourceError::Io, &source))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|source| classified(SourceError::Io, &source))?;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    if !metadata.file_type().is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > maximum as u64
    {
        return Err(SourceError::Insecure);
    }
    let mut bytes = Vec::new();
    file.take((maximum + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| classified(SourceError::Io, &source))?;
    if bytes.len() > maximum {
        return Err(SourceError::TooLarge);
    }
    Ok(bytes)
}

async fn read_kubernetes_projection(root: &Path, key: &str) -> Result<Vec<u8>, SourceError> {
    validate_absolute(root).map_err(|source| classified(SourceError::Config, &source))?;
    validate_one_component(key).map_err(|source| classified(SourceError::Config, &source))?;
    let root_metadata = tokio::fs::symlink_metadata(root)
        .await
        .map_err(|source| classified(SourceError::Io, &source))?;
    use std::os::unix::fs::PermissionsExt as _;
    if !root_metadata.is_dir() {
        return Err(SourceError::Insecure);
    }
    if root_metadata.permissions().mode() & 0o022 != 0 {
        // Kubelet's real AtomicWriter root is commonly 01777. It is safe only when the pod mounted
        // that volume read-only; accepting the mode on a writable filesystem would let another
        // process replace `..data` between snapshots. The chart always sets readOnly: true.
        let filesystem =
            rustix::fs::statvfs(root).map_err(|source| classified(SourceError::Io, &source))?;
        if !filesystem
            .f_flag
            .contains(rustix::fs::StatVfsMountFlags::RDONLY)
        {
            return Err(SourceError::Insecure);
        }
    }
    let target = tokio::fs::read_link(root.join("..data"))
        .await
        .map_err(|source| classified(SourceError::Io, &source))?;
    if target.components().count() != 1
        || !matches!(target.components().next(), Some(Component::Normal(_)))
    {
        return Err(SourceError::Insecure);
    }
    let generation = root.join(target);
    let generation_metadata = tokio::fs::symlink_metadata(&generation)
        .await
        .map_err(|source| classified(SourceError::Io, &source))?;
    if !generation_metadata.is_dir() || generation_metadata.file_type().is_symlink() {
        return Err(SourceError::Insecure);
    }
    let path = generation.join(key);
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .await
        .map_err(|source| classified(SourceError::Io, &source))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|source| classified(SourceError::Io, &source))?;
    if !metadata.is_file() || metadata.len() > HARD_MAX_SOURCE_RESPONSE_BYTES as u64 {
        return Err(SourceError::Insecure);
    }
    let mut bytes = Vec::new();
    file.take((HARD_MAX_SOURCE_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|source| classified(SourceError::Io, &source))?;
    if bytes.len() > HARD_MAX_SOURCE_RESPONSE_BYTES {
        return Err(SourceError::TooLarge);
    }
    Ok(bytes)
}

async fn bounded_json(
    request: reqwest::RequestBuilder,
    timeout_ms: u64,
) -> Result<Value, SourceError> {
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(timeout_ms))
        .ok_or(SourceError::Config)?;
    let response = timeout_at(deadline, request.send())
        .await
        .map_err(|source| classified(SourceError::Timeout, &source))?
        .map_err(|source| classified(SourceError::Transport, &source))?;
    if !response.status().is_success() {
        tracing::debug!(
            event = "secret_source_cause_classified",
            category = "rejected",
            cause_type = "http-status",
            http.response.status_code = response.status().as_u16(),
        );
        return Err(if response.status() == reqwest::StatusCode::NOT_FOUND {
            SourceError::Missing
        } else {
            SourceError::Rejected
        });
    }
    if response.headers().len() > MAX_SOURCE_RESPONSE_HEADERS
        || response
            .headers()
            .iter()
            .try_fold(0_usize, |size, (name, value)| {
                size.checked_add(name.as_str().len())?
                    .checked_add(value.as_bytes().len())?
                    .checked_add(4)
            })
            .is_none_or(|size| size > MAX_SOURCE_RESPONSE_HEADER_BYTES)
    {
        return Err(SourceError::TooLarge);
    }
    if response
        .content_length()
        .is_some_and(|length| length > HARD_MAX_SOURCE_RESPONSE_BYTES as u64)
    {
        return Err(SourceError::TooLarge);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = timeout_at(deadline, stream.next())
        .await
        .map_err(|source| classified(SourceError::Timeout, &source))?
    {
        let chunk = chunk.map_err(|source| classified(SourceError::Transport, &source))?;
        if bytes.len().saturating_add(chunk.len()) > HARD_MAX_SOURCE_RESPONSE_BYTES {
            return Err(SourceError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let value = StrictDocument::deserialize(&mut deserializer)
        .map_err(|source| classified(SourceError::Malformed, &source))?
        .0;
    deserializer
        .end()
        .map_err(|source| classified(SourceError::Malformed, &source))?;
    validate_document(&value)?;
    Ok(value)
}

struct StrictDocument(Value);

impl<'de> Deserialize<'de> for StrictDocument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StrictVisitor;

        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictDocument;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded JSON/YAML value with unique string keys")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(StrictDocument(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(StrictDocument(Value::Number(value.into())))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(StrictDocument(Value::Number(value.into())))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .map(StrictDocument)
                    .ok_or_else(|| E::custom("non-finite number is not supported"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(StrictDocument(Value::String(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(StrictDocument(Value::String(value)))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(StrictDocument(Value::Null))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(StrictDocument(Value::Null))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<StrictDocument>()? {
                    values.push(value.0);
                }
                Ok(StrictDocument(Value::Array(values)))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate document key {key:?}"
                        )));
                    }
                    let value = map.next_value::<StrictDocument>()?;
                    values.insert(key, value.0);
                }
                Ok(StrictDocument(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(StrictVisitor)
    }
}

fn validate_document(value: &Value) -> Result<(), SourceError> {
    let mut entries = 0_usize;
    validate_document_bounds(value, 0, &mut entries)
}

fn validate_document_bounds(
    value: &Value,
    depth: usize,
    entries: &mut usize,
) -> Result<(), SourceError> {
    if depth > 32 || *entries > 4096 {
        return Err(SourceError::TooLarge);
    }
    *entries = (*entries).saturating_add(1);
    match value {
        Value::Array(values) => {
            for value in values {
                validate_document_bounds(value, depth + 1, entries)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if key.len() > 4096 {
                    return Err(SourceError::TooLarge);
                }
                validate_document_bounds(value, depth + 1, entries)?;
            }
        }
        Value::String(value) if value.len() > MAX_DOCUMENT_SCALAR_BYTES => {
            return Err(SourceError::TooLarge);
        }
        _ => {}
    }
    Ok(())
}

fn project(bytes: Vec<u8>, projection: &Projection) -> Result<Vec<u8>, SourceError> {
    let selected = match projection.format {
        DocumentFormat::Raw => bytes,
        DocumentFormat::Utf8 => {
            std::str::from_utf8(&bytes)
                .map_err(|source| classified(SourceError::Malformed, &source))?;
            bytes
        }
        DocumentFormat::Json => {
            let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
            let value = StrictDocument::deserialize(&mut deserializer)
                .map_err(|source| classified(SourceError::Malformed, &source))?
                .0;
            deserializer
                .end()
                .map_err(|source| classified(SourceError::Malformed, &source))?;
            validate_document(&value)?;
            selected_document_scalar(&value, projection.pointer.as_deref())?
        }
        DocumentFormat::Yaml => {
            // The generic serde data model erases whether a value came through an alias or custom
            // tag. Reject their syntax before parsing so a small source cannot expand into a large
            // alias graph, and so a tag cannot change scalar interpretation behind the selector.
            // This deliberately conservative grammar may reject these bytes inside quoted YAML;
            // JSON or raw projection is the escape hatch for such values.
            if bytes.iter().any(|byte| matches!(byte, b'&' | b'*' | b'!')) {
                return Err(SourceError::Malformed);
            }
            let value = serde_yaml::from_slice::<StrictDocument>(&bytes)
                .map_err(|source| classified(SourceError::Malformed, &source))?
                .0;
            validate_document(&value)?;
            selected_document_scalar(&value, projection.pointer.as_deref())?
        }
    };
    let result = if projection.decode_base64 {
        let text = std::str::from_utf8(&selected)
            .map_err(|source| classified(SourceError::Malformed, &source))?;
        STANDARD
            .decode(text)
            .map_err(|source| classified(SourceError::Malformed, &source))?
    } else {
        selected
    };
    if result.is_empty() {
        return Err(SourceError::Empty);
    }
    if result.len() > HARD_MAX_SECRET_BYTES {
        return Err(SourceError::TooLarge);
    }
    Ok(result)
}

fn valid_json_pointer(pointer: &str) -> bool {
    if pointer.len() > 4096 || (!pointer.is_empty() && !pointer.starts_with('/')) {
        return false;
    }
    let mut bytes = pointer.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'~'
            && !bytes
                .next()
                .is_some_and(|escaped| matches!(escaped, b'0' | b'1'))
        {
            return false;
        }
    }
    true
}

fn selected_document_scalar(value: &Value, pointer: Option<&str>) -> Result<Vec<u8>, SourceError> {
    let value = match pointer {
        Some(pointer) => {
            if !pointer.is_empty() && !pointer.starts_with('/') {
                return Err(SourceError::Config);
            }
            value.pointer(pointer).ok_or(SourceError::Missing)?
        }
        None => value,
    };
    scalar_bytes(value)
}

fn reject_bootstrap_reflection(
    material: Vec<u8>,
    bootstrap_values: &[&str],
) -> Result<Vec<u8>, SourceError> {
    if bootstrap_values.iter().any(|bootstrap| {
        !bootstrap.is_empty()
            && material
                .windows(bootstrap.len())
                .any(|window| window == bootstrap.as_bytes())
    }) {
        return Err(SourceError::BootstrapReflected);
    }
    Ok(material)
}

fn scalar_bytes(value: &Value) -> Result<Vec<u8>, SourceError> {
    value
        .as_str()
        .map(|value| value.as_bytes().to_vec())
        .ok_or(SourceError::Malformed)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AwsCredentialsFile {
    access_key_id: String,
    secret_access_key: Redacted<String>,
    #[serde(default)]
    session_token: Option<Redacted<String>>,
}

async fn read_aws_credentials(path: &Path, uid: u32) -> Result<AwsCredentialsFile, SourceError> {
    let bytes = read_private_file(path, uid, 64 * 1024).await?;
    let credentials: AwsCredentialsFile = serde_yaml::from_slice(&bytes)
        .map_err(|source| classified(SourceError::Malformed, &source))?;
    if credentials.access_key_id.is_empty()
        || credentials.access_key_id.len() > 256
        || credentials.secret_access_key.expose().is_empty()
        || credentials.secret_access_key.expose().len() > MAX_BOOTSTRAP_TOKEN_BYTES
        || credentials
            .access_key_id
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || credentials
            .secret_access_key
            .expose()
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || credentials.session_token.as_ref().is_some_and(|token| {
            token.expose().is_empty()
                || token.expose().len() > MAX_BOOTSTRAP_TOKEN_BYTES
                || token
                    .expose()
                    .bytes()
                    .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        })
    {
        return Err(SourceError::Malformed);
    }
    Ok(credentials)
}

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f6_3b78 & (0_u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn hmac(key: &[u8], value: &[u8]) -> Result<Vec<u8>, SourceError> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|source| classified(SourceError::Internal, &source))?;
    mac.update(value);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn escape_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("source configuration is invalid")]
    Config,
    #[error("source file is insecure")]
    Insecure,
    #[error("source I/O failed")]
    Io,
    #[error("source request timed out")]
    Timeout,
    #[error("source transport failed")]
    Transport,
    #[error("source rejected the request")]
    Rejected,
    #[error("source value is missing")]
    Missing,
    #[error("source response failed an integrity check")]
    Integrity,
    #[error("source response reflected its bootstrap credential")]
    BootstrapReflected,
    #[error("source value is empty")]
    Empty,
    #[error("source response is malformed")]
    Malformed,
    #[error("source response is too large")]
    TooLarge,
    #[error("internal source operation failed")]
    Internal,
}

fn classified<E: fmt::Debug + 'static>(error: SourceError, source: &E) -> SourceError {
    let any = source as &dyn std::any::Any;
    if let Some(source) = any.downcast_ref::<std::io::Error>() {
        tracing::debug!(
            event = "secret_source_cause_classified",
            category = error.category(),
            cause_type = "io",
            error.kind = ?source.kind(),
            error.errno = source.raw_os_error(),
        );
    } else if let Some(source) = any.downcast_ref::<reqwest::Error>() {
        tracing::debug!(
            event = "secret_source_cause_classified",
            category = error.category(),
            cause_type = "http",
            error.timeout = source.is_timeout(),
            error.connect = source.is_connect(),
            http.response.status_code = source.status().map(|status| status.as_u16()),
        );
    } else if let Some(source) = any.downcast_ref::<serde_json::Error>() {
        tracing::debug!(
            event = "secret_source_cause_classified",
            category = error.category(),
            cause_type = "json",
            error.class = ?source.classify(),
            error.line = source.line(),
            error.column = source.column(),
        );
    } else {
        // Some dependency errors carry secret-derived byte offsets or private endpoints when
        // rendered. Their concrete type plus the stable category is the safe cause at this site.
        tracing::debug!(
            event = "secret_source_cause_classified",
            category = error.category(),
            cause_type = std::any::type_name::<E>(),
        );
    }
    error
}

impl SourceError {
    const fn category(&self) -> &'static str {
        match self {
            Self::Config => "configuration",
            Self::Insecure => "insecure-file",
            Self::Io => "io",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::Rejected => "rejected",
            Self::Missing => "missing",
            Self::Integrity => "integrity",
            Self::BootstrapReflected => "bootstrap-reflected",
            Self::Empty => "empty",
            Self::Malformed => "malformed",
            Self::TooLarge => "too-large",
            Self::Internal => "internal",
        }
    }
}

#[derive(Debug, Error)]
pub enum SecretMapError {
    #[error("could not read private secret map at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: SourceError,
    },
    #[error("private secret map is not strict valid YAML or JSON")]
    Decode {
        #[source]
        source: serde_yaml::Error,
    },
    #[error("private secret map has conflicts: {}", problems.join("; "))]
    Validation { problems: Vec<String> },
    #[error("private secret map HTTP client could not be built")]
    Client {
        #[source]
        source: reqwest::Error,
    },
    #[error("private secret catalog could not be built")]
    Catalog {
        #[source]
        source: BrokerBuildError,
    },
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read as _, Write as _},
        net::TcpListener,
        os::unix::fs::PermissionsExt as _,
        sync::mpsc,
        thread,
    };

    use super::{DocumentFormat, MapResolver, Projection, SecretMapError, SecretSource, load};

    fn uid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    async fn write(path: &std::path::Path, bytes: &[u8]) {
        tokio::fs::write(path, bytes).await.expect("write fixture");
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .expect("chmod fixture");
    }

    fn mock_json(body: &str) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let authority = listener.local_addr().expect("address").to_string();
        let body = body.to_owned();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut bytes = vec![0_u8; 32 * 1024];
            let read = stream.read(&mut bytes).expect("read request");
            bytes.truncate(read);
            sender
                .send(String::from_utf8(bytes).expect("request text"))
                .expect("record request");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        (format!("http://{authority}"), receiver, handle)
    }

    fn resolver() -> MapResolver {
        MapResolver {
            entries: std::collections::BTreeMap::new(),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .build()
                .expect("client"),
            expected_uid: uid(),
        }
    }

    #[test]
    fn private_map_camel_case_contract_decodes_every_current_source_variant() {
        let yaml = r#"
apiVersion: dekopon.dev/secret-map/v1alpha1
mapRevision: all-sources
secrets:
  - { drn: drn:com.xrl:secret:test:file/value, source: { kind: secureFile, path: /run/value }, bindings: [] }
  - drn: drn:com.xrl:secret:test:projection/value
    source: { kind: kubernetesProjection, root: /run/projected, key: value, declaredOrigin: configMap, acknowledgeNonSecretSource: true }
    bindings: []
  - drn: drn:com.xrl:secret:test:op/value
    source: { kind: onePasswordConnect, endpoint: https://op.example, tokenFile: /run/op, vault: v, item: i, field: f, timeoutMs: 1000 }
    bindings: []
  - drn: drn:com.xrl:secret:test:vault1/value
    source: { kind: vaultKv1, endpoint: https://vault.example, tokenFile: /run/vault, mount: secret, path: app, key: value }
    bindings: []
  - drn: drn:com.xrl:secret:test:vault2/value
    source: { kind: vaultKv2, endpoint: https://vault.example, tokenFile: /run/vault, namespace: team, mount: secret, path: app, key: value, version: 2 }
    bindings: []
  - drn: drn:com.xrl:secret:test:aws/value
    source: { kind: awsSecretsManager, region: us-east-1, credentialsFile: /run/aws, secretId: arn:example, versionStage: AWSCURRENT }
    bindings: []
  - drn: drn:com.xrl:secret:test:ssm/value
    source: { kind: awsSsmParameter, region: us-east-1, credentialsFile: /run/aws, name: /app/value, selector: CURRENT }
    bindings: []
  - drn: drn:com.xrl:secret:test:gcp/value
    source: { kind: gcpSecretManager, endpoint: https://secretmanager.googleapis.com, tokenFile: /run/gcp, project: p, location: us-central1, secret: app, version: latest }
    bindings: []
  - drn: drn:com.xrl:secret:test:azure/value
    source: { kind: azureKeyVault, vaultUrl: https://vault.azure.net, tokenFile: /run/azure, secret: app, version: v1 }
    bindings: []
  - drn: drn:com.xrl:secret:test:kube/value
    source: { kind: kubernetesApi, endpoint: https://kubernetes.default.svc, tokenFile: /run/kube, namespace: default, objectKind: configMap, name: app, key: value, acknowledgeNonSecretSource: true }
    bindings: []
"#;
        let parsed = serde_yaml::from_str::<super::SecretMapFile>(yaml)
            .expect("documented camelCase source fields decode");
        assert_eq!(parsed.secrets.len(), 10);
    }

    #[tokio::test]
    async fn secure_file_map_resolves_only_after_catalog_construction() {
        let directory = tempfile::tempdir().expect("tempdir");
        let secret = directory.path().join("secret");
        write(&secret, b"fixture-token").await;
        let map = directory.path().join("map.yaml");
        let document = format!(
            "apiVersion: dekopon.dev/secret-map/v1alpha1\nmapRevision: test\nsecrets:\n  - drn: drn:com.xrl:secret:test:api/token\n    source: {{ kind: secureFile, path: {} }}\n    bindings:\n      - id: api-token\n        capability: http-probe.fetch\n        sink: httpBearer\n        allowedHosts: [api.example.test]\n        allowedMethods: [GET]\n        allowedPaths: [{{ match: exact, path: /v1/thing }}]\n",
            secret.display()
        );
        write(&map, document.as_bytes()).await;
        let catalog = load(&map, uid()).await.expect("load map");
        assert_eq!(catalog.drns().count(), 1);
    }

    #[tokio::test]
    async fn remote_adapters_decode_their_native_response_shapes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let token = directory.path().join("token");
        write(&token, b"bootstrap-token\n").await;
        let aws = directory.path().join("aws.yaml");
        write(
            &aws,
            b"accessKeyId: AKIDEXAMPLE\nsecretAccessKey: fixture-secret-key\nsessionToken: fixture-session\n",
        )
        .await;
        let resolver = resolver();

        let (endpoint, request, server) =
            mock_json(r#"{"fields":[{"id":"password","label":"password","value":"op-value"}]}"#);
        let value = resolver
            .resolve_source(&SecretSource::OnePasswordConnect {
                endpoint,
                token_file: token.clone(),
                vault: "vault-id".to_owned(),
                item: "item-id".to_owned(),
                field: "password".to_owned(),
                timeout_ms: 5_000,
            })
            .await
            .expect("1Password response");
        assert_eq!(value, b"op-value");
        assert!(
            request
                .recv()
                .expect("request")
                .contains("authorization: Bearer bootstrap-token")
        );
        server.join().expect("server");

        let (endpoint, request, server) =
            mock_json(r#"{"data":{"data":{"password":"vault-value"}}}"#);
        let value = resolver
            .resolve_source(&SecretSource::VaultKv2 {
                endpoint,
                token_file: token.clone(),
                namespace: Some("tenant".to_owned()),
                mount: "secret".to_owned(),
                path: "apps/api".to_owned(),
                key: "password".to_owned(),
                version: None,
                timeout_ms: 5_000,
            })
            .await
            .expect("Vault v2 response");
        assert_eq!(value, b"vault-value");
        let request = request.recv().expect("request");
        assert!(request.contains("/v1/secret/data/apps/api"), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-vault-namespace: tenant"),
            "{request}"
        );
        server.join().expect("server");

        let (endpoint, request, server) = mock_json(r#"{"SecretString":"aws-value"}"#);
        let value = resolver
            .resolve_source(&SecretSource::AwsSecretsManager {
                endpoint: Some(endpoint),
                region: "us-east-1".to_owned(),
                credentials_file: aws.clone(),
                secret_id: "arn:aws:secretsmanager:us-east-1:123:secret:test".to_owned(),
                version_id: None,
                version_stage: Some("AWSCURRENT".to_owned()),
                timeout_ms: 5_000,
            })
            .await
            .expect("AWS response");
        assert_eq!(value, b"aws-value");
        let request = request.recv().expect("request");
        assert!(
            request.contains("x-amz-target: secretsmanager.GetSecretValue"),
            "{request}"
        );
        assert!(
            request.contains("authorization: AWS4-HMAC-SHA256"),
            "{request}"
        );
        server.join().expect("server");

        let (endpoint, request, server) = mock_json(r#"{"Parameter":{"Value":"ssm-value"}}"#);
        let value = resolver
            .resolve_source(&SecretSource::AwsSsmParameter {
                endpoint: Some(endpoint),
                region: "us-east-1".to_owned(),
                credentials_file: aws,
                name: "/prod/api/password".to_owned(),
                selector: Some("CURRENT".to_owned()),
                timeout_ms: 5_000,
            })
            .await
            .expect("SSM response");
        assert_eq!(value, b"ssm-value");
        let request = request.recv().expect("request");
        assert!(
            request.contains("x-amz-target: AmazonSSM.GetParameter"),
            "{request}"
        );
        server.join().expect("server");

        let (endpoint, request, server) =
            mock_json(r#"{"payload":{"data":"Z2NwLXZhbHVl","dataCrc32c":"1275512963"}}"#);
        let value = resolver
            .resolve_source(&SecretSource::GcpSecretManager {
                endpoint: Some(endpoint),
                token_file: token.clone(),
                project: "project-1".to_owned(),
                location: Some("us-central1".to_owned()),
                secret: "api".to_owned(),
                version: "latest".to_owned(),
                timeout_ms: 5_000,
            })
            .await
            .expect("GCP response");
        assert_eq!(value, b"gcp-value");
        let request = request.recv().expect("request");
        assert!(
            request.contains(
                "/projects/project-1/locations/us-central1/secrets/api/versions/latest:access"
            ),
            "{request}"
        );
        server.join().expect("server");

        let (endpoint, _request, server) = mock_json(r#"{"value":"azure-value"}"#);
        let value = resolver
            .resolve_source(&SecretSource::AzureKeyVault {
                vault_url: endpoint,
                token_file: token.clone(),
                secret: "api".to_owned(),
                version: Some("version-1".to_owned()),
                timeout_ms: 5_000,
            })
            .await
            .expect("Azure response");
        assert_eq!(value, b"azure-value");
        server.join().expect("server");

        let (endpoint, _request, server) = mock_json(r#"{"data":{"password":"a3ViZS12YWx1ZQ=="}}"#);
        let value = resolver
            .resolve_source(&SecretSource::KubernetesApi {
                endpoint,
                token_file: token,
                namespace: "default".to_owned(),
                object_kind: super::KubernetesObjectKind::Secret,
                name: "api".to_owned(),
                key: "password".to_owned(),
                acknowledge_non_secret_source: false,
                timeout_ms: 5_000,
            })
            .await
            .expect("Kubernetes response");
        assert_eq!(value, b"kube-value");
        server.join().expect("server");
    }

    #[test]
    fn crc32c_matches_the_standard_check_vector() {
        assert_eq!(super::crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn backend_bootstrap_values_cannot_become_resolved_application_material() {
        let error = super::reject_bootstrap_reflection(
            b"prefix-bootstrap-token-suffix".to_vec(),
            &["bootstrap-token"],
        )
        .expect_err("bootstrap reflection refuses");
        assert_eq!(error.category(), "bootstrap-reflected");
    }

    #[test]
    fn strict_document_projection_rejects_duplicate_json_and_yaml_keys() {
        for (format, bytes) in [
            (
                DocumentFormat::Json,
                br#"{"password":"one","password":"two"}"#.as_slice(),
            ),
            (
                DocumentFormat::Yaml,
                b"password: one\npassword: two\n".as_slice(),
            ),
            (
                DocumentFormat::Yaml,
                b"value: &shared one\npassword: *shared\n".as_slice(),
            ),
        ] {
            let error = super::project(
                bytes.to_vec(),
                &Projection {
                    format,
                    pointer: Some("/password".to_owned()),
                    decode_base64: false,
                },
            )
            .expect_err("duplicate keys refuse");
            assert_eq!(error.category(), "malformed");
        }
    }

    #[tokio::test]
    async fn kubernetes_projection_reads_one_atomic_writer_generation_at_a_time() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path().join("projection");
        tokio::fs::create_dir(&root).await.expect("root");
        for (generation, value) in [
            ("..gen-one", b"one".as_slice()),
            ("..gen-two", b"two".as_slice()),
        ] {
            let path = root.join(generation);
            tokio::fs::create_dir(&path).await.expect("generation");
            tokio::fs::write(path.join("password"), value)
                .await
                .expect("projected value");
        }
        symlink("..gen-one", root.join("..data")).expect("initial data link");
        assert_eq!(
            super::read_kubernetes_projection(&root, "password")
                .await
                .expect("first snapshot"),
            b"one"
        );
        let replacement = root.join("..data-next");
        symlink("..gen-two", &replacement).expect("replacement link");
        std::fs::rename(replacement, root.join("..data")).expect("atomic swap");
        assert_eq!(
            super::read_kubernetes_projection(&root, "password")
                .await
                .expect("second snapshot"),
            b"two"
        );
    }

    #[tokio::test]
    async fn map_and_backend_bootstrap_files_cannot_become_agent_selected_material() {
        let directory = tempfile::tempdir().expect("tempdir");
        let map = directory.path().join("map.yaml");
        let token = directory.path().join("bootstrap-token");
        write(&token, b"bootstrap").await;
        let document = format!(
            "apiVersion: dekopon.dev/secret-map/v1alpha1\nmapRevision: test\nsecrets:\n  - drn: drn:com.xrl:secret:test:map/value\n    source: {{ kind: secureFile, path: {} }}\n    bindings: []\n  - drn: drn:com.xrl:secret:test:bootstrap/value\n    source: {{ kind: secureFile, path: {} }}\n    bindings: []\n  - drn: drn:com.xrl:secret:test:remote/value\n    source:\n      kind: onePasswordConnect\n      endpoint: https://connect.example.test\n      tokenFile: {}\n      vault: vault\n      item: item\n      field: password\n    bindings: []\n",
            map.display(),
            token.display(),
            token.display()
        );
        write(&map, document.as_bytes()).await;
        let error = super::load(&map, uid())
            .await
            .expect_err("collisions refuse");
        let SecretMapError::Validation { problems } = error else {
            panic!("unexpected: {error:?}");
        };
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("map cannot be used")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("bootstrap credential")),
            "{problems:?}"
        );
    }

    #[tokio::test]
    async fn private_map_rejects_symlinks_and_group_readability() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("tempdir");
        let target = directory.path().join("map.yaml");
        write(
            &target,
            b"apiVersion: dekopon.dev/secret-map/v1alpha1\nmapRevision: test\nsecrets: []\n",
        )
        .await;
        let link = directory.path().join("link.yaml");
        symlink(&target, &link).expect("symlink");
        assert!(super::load(&link, uid()).await.is_err());
        tokio::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640))
            .await
            .expect("chmod");
        assert!(super::load(&target, uid()).await.is_err());
    }

    #[tokio::test]
    async fn validation_reports_multiple_conflicts_together() {
        let directory = tempfile::tempdir().expect("tempdir");
        let map = directory.path().join("map.yaml");
        let document = "apiVersion: dekopon.dev/secret-map/v1alpha1\nmapRevision: ''\nsecrets:\n  - drn: drn:com.xrl:secret:test:a/value\n    source: { kind: secureFile, path: relative }\n    bindings: []\n  - drn: drn:com.xrl:secret:test:a/value\n    source: { kind: secureFile, path: also-relative }\n    bindings: []\n";
        write(&map, document.as_bytes()).await;
        let error = load(&map, uid()).await.expect_err("conflicts refuse");
        let SecretMapError::Validation { problems } = error else {
            panic!("unexpected error: {error:?}");
        };
        assert!(problems.len() >= 4, "{problems:?}");
    }
}
