use std::collections::BTreeSet;

use dekopon_broker_host::{BoundCredential, EgressPushHeaders};
use dekopon_broker_protocol::{Grant, Tally};
use dekopon_core::{
    CapabilityId, Redacted, SecretBytes, SecretDrn, SecretSinkKind,
    base64::{Engine as _, URL_SAFE_NO_PAD},
};
use dekopon_policy::SecretSelection;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const WIRE_VERSION: &str = "v1.";
const MAX_EGRESS_HEADER_BYTES: usize = 16_384;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EgressProfile {
    pub drn: SecretDrn,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CookieEntry {
    pub host: String,
    pub name: String,
    #[serde(serialize_with = "dekopon_core::serialize_exposed")]
    pub value: Redacted<String>,
    pub seed: u64,
    pub rev: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CookieSet(Vec<CookieEntry>);

impl CookieSet {
    pub fn new(entries: Vec<CookieEntry>) -> Result<Self, EgressError> {
        let mut seen = BTreeSet::new();
        if entries
            .iter()
            .all(|entry| seen.insert((entry.host.as_str(), entry.name.as_str())))
        {
            Ok(Self(entries))
        } else {
            Err(EgressError::DuplicateCookie)
        }
    }

    #[must_use]
    pub fn entries(&self) -> &[CookieEntry] {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "form", rename_all = "camelCase", deny_unknown_fields)]
pub enum TokenTarget {
    Bearer {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paths: Option<Vec<String>>,
    },
    Token {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paths: Option<Vec<String>>,
    },
    Basic {
        host: String,
        user: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paths: Option<Vec<String>>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PushedToken {
    #[serde(serialize_with = "dekopon_core::serialize_exposed")]
    pub token: Redacted<String>,
    pub on: Vec<TokenTarget>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EgressProfileMaterial {
    pub cookies: CookieSet,
    pub tokens: Vec<PushedToken>,
}

impl EgressProfileMaterial {
    fn pushed_secrets(&self) -> Vec<SecretBytes> {
        self.tokens
            .iter()
            .map(|token| token.token.expose().as_bytes())
            .chain(
                self.cookies
                    .entries()
                    .iter()
                    .map(|cookie| cookie.value.expose().as_bytes()),
            )
            .map(|bytes| SecretBytes::new(bytes.to_vec()))
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressPush {
    pub profile: SecretDrn,
    pub grant: Option<Grant>,
    pub material: EgressProfileMaterial,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PushWire<'a> {
    profile: &'a SecretDrn,
    #[serde(skip_serializing_if = "Option::is_none")]
    grant: Option<&'a Grant>,
    cookies: &'a [CookieEntry],
    tokens: &'a [PushedToken],
}

impl EgressPush {
    pub fn encode(&self) -> Result<EgressHeaderValue, EgressError> {
        let json = serde_json::to_vec(&PushWire {
            profile: &self.profile,
            grant: self.grant.as_ref(),
            cookies: self.material.cookies.entries(),
            tokens: &self.material.tokens,
        })
        .map_err(|_error| EgressError::Encode)?;
        if WIRE_VERSION.len() + (json.len() * 4).div_ceil(3) > MAX_EGRESS_HEADER_BYTES {
            return Err(EgressError::PushTooLarge {
                profile: self.profile.clone(),
            });
        }
        let mut value = String::with_capacity(MAX_EGRESS_HEADER_BYTES);
        value.push_str(WIRE_VERSION);
        URL_SAFE_NO_PAD.encode_string(&json, &mut value);
        Ok(EgressHeaderValue(Redacted::new(value)))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressHeaderValue(Redacted<String>);

impl EgressHeaderValue {
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.expose().as_str()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EgressReturn {
    NotApplied,
    Applied {
        rotations: Vec<CookieEntry>,
        tallies: Vec<Tally>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ReturnWire {
    applied: bool,
    #[serde(default)]
    rotations: Vec<CookieEntry>,
    #[serde(default)]
    tallies: Vec<Tally>,
}

impl EgressReturn {
    #[must_use]
    pub fn decode(value: &[u8]) -> Option<Self> {
        let encoded = value.strip_prefix(WIRE_VERSION.as_bytes())?;
        if encoded.len() > MAX_EGRESS_HEADER_BYTES {
            return None;
        }
        let json = URL_SAFE_NO_PAD.decode(encoded).ok()?;
        let wire: ReturnWire = serde_json::from_slice(&json).ok()?;
        Some(if wire.applied {
            Self::Applied {
                rotations: wire.rotations,
                tallies: wire.tallies,
            }
        } else {
            Self::NotApplied
        })
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum EgressError {
    #[error("the egress push for profile {profile} exceeds the header bound")]
    PushTooLarge { profile: SecretDrn },
    #[error("policy does not permit pushing egress profile {profile}")]
    Denied { profile: SecretDrn },
    #[error("an egress profile needs a secret-bound runner credential")]
    Unbound,
    #[error("a cookie set names one cookie twice")]
    DuplicateCookie,
    #[error("the egress push could not be encoded")]
    Encode,
}

impl EgressError {
    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            Self::PushTooLarge { .. } => "egress-push-too-large",
            Self::Denied { .. } => "egress-denied",
            Self::Unbound => "egress-unbound",
            Self::DuplicateCookie => "egress-duplicate-cookie",
            Self::Encode => "egress-encode",
        }
    }
}

impl<A> crate::Broker<A>
where
    A: crate::AuditLog,
{
    pub(crate) async fn egress_push(
        &self,
        context: &crate::AuthenticatedContext,
        capability: &CapabilityId,
        set: &crate::ConstraintSet,
        grant: Option<Grant>,
    ) -> Result<Option<(EgressPushHeaders, Vec<SecretBytes>)>, crate::CredentialFailure> {
        let Some(profile) = self
            .constraints
            .effective_egress_profile(set, context.actor())
        else {
            return Ok(None);
        };
        if !self
            .authorize_secret_sink(
                context,
                capability,
                set,
                (profile, SecretSinkKind::VmEgressProfile),
                SecretSelection::Default,
            )
            .allowed
        {
            return Err(EgressError::Denied {
                profile: profile.clone(),
            }
            .into());
        }
        let mut push = EgressPush {
            profile: profile.clone(),
            grant: None,
            material: self.secrets.resolver.egress_profile(profile).await?,
        };
        let every = push.encode()?;
        let exec = match grant {
            Some(grant) => {
                push.grant = Some(grant);
                push.encode()?
            }
            None => every.clone(),
        };
        Ok(Some((
            EgressPushHeaders {
                every: every.0,
                exec: exec.0,
            },
            push.material.pushed_secrets(),
        )))
    }

    pub(crate) async fn attach_egress_push(
        &self,
        context: &crate::AuthenticatedContext,
        capability: &CapabilityId,
        set: &crate::ConstraintSet,
        grant: Option<Grant>,
        credential: Option<BoundCredential>,
    ) -> Result<Option<BoundCredential>, crate::CredentialFailure> {
        let Some((headers, pushed)) = self.egress_push(context, capability, set, grant).await?
        else {
            return Ok(credential);
        };
        let credential = credential.ok_or(EgressError::Unbound)?;
        Ok(Some(credential.with_egress_push(headers, pushed)?))
    }
}
