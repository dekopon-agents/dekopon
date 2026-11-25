#![expect(
    clippy::todo,
    reason = "skeleton: egress/push-contract, egress/vault-rotation and state/grant-settle fill these bodies"
)]

use dekopon_broker_protocol::{Grant, GrantId, GrantSettlement, Tally};
use dekopon_core::{Redacted, SecretDrn, VmSession};
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    pub fn new(_entries: Vec<CookieEntry>) -> Result<Self, EgressError> {
        todo!()
    }

    #[must_use]
    pub fn entries(&self) -> &[CookieEntry] {
        &self.0
    }

    #[must_use]
    pub fn measured_bytes(&self) -> usize {
        todo!()
    }

    pub fn storable(&self, _rotations: Vec<CookieEntry>) -> Result<Self, EgressError> {
        todo!()
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressPush {
    pub profile: SecretDrn,
    pub grant: Option<Grant>,
    pub material: EgressProfileMaterial,
}

impl EgressPush {
    pub fn encode(&self) -> Result<EgressHeaderValue, EgressError> {
        todo!()
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

impl EgressReturn {
    #[must_use]
    pub fn decode(_value: &[u8]) -> Option<Self> {
        todo!()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecWrite {
    NotWritten,
    Written,
}

#[must_use]
pub fn settle(
    _grant: Option<GrantId>,
    _session: Option<VmSession>,
    _exec: ExecWrite,
    _answer: Option<&EgressReturn>,
) -> GrantSettlement {
    todo!()
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum EgressError {
    #[error("the egress push for profile {profile} exceeds the header bound")]
    PushTooLarge { profile: SecretDrn },
    #[error("the cookie set of profile {profile} exceeds its budget")]
    CookieSetOverBudget { profile: SecretDrn },
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
            Self::CookieSetOverBudget { .. } => "egress-cookie-budget",
            Self::DuplicateCookie => "egress-duplicate-cookie",
            Self::Encode => "egress-encode",
        }
    }
}

impl<A> crate::Broker<A>
where
    A: crate::AuditLog,
{
    #[expect(
        dead_code,
        reason = "skeleton: state/grant-settle calls it from the invoke path"
    )]
    pub(crate) fn grant_refusal(
        &self,
        _set: &crate::ConstraintSet,
        _grant: Option<&Grant>,
    ) -> Option<dekopon_broker_protocol::RunOutcome> {
        todo!()
    }

    #[expect(
        dead_code,
        reason = "skeleton: egress/push-contract calls it from resolve_credential"
    )]
    #[expect(
        clippy::manual_async_fn,
        reason = "the signature states the Send bound the serving task needs"
    )]
    pub(crate) fn egress_push(
        &self,
        _set: &crate::ConstraintSet,
        _grant: Option<Grant>,
    ) -> impl Future<Output = Result<Option<EgressPush>, EgressError>> + Send {
        async { todo!() }
    }
}
