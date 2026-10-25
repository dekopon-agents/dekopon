use dekopon_agent::BrokerLegError;
use dekopon_broker_protocol::{ClientError, ExchangePhase, ProtocolError};

use super::{ErrorReport, broker_refusal, chain};
use crate::session::SessionError;

fn refused() -> SessionError {
    SessionError::Leg(BrokerLegError::Client(ClientError::Remote {
        code: "unauthenticated".into(),
        message: "attestation refused: no attestor authority for this subject".into(),
    }))
}

fn undecodable() -> SessionError {
    let source = serde_json::from_str::<u8>("\"not a number\"").unwrap_err();
    SessionError::Leg(BrokerLegError::Client(ClientError::Protocol {
        phase: ExchangePhase::Response,
        source: ProtocolError::Deserialize { source },
    }))
}

#[test]
fn chain_follows_every_source_like_anyhow_alternate() {
    let text = chain(&undecodable());
    assert_eq!(
        text,
        "could not open a broker session for this subject and agent: broker response framing \
         failed: broker frame is not valid protocol JSON: invalid type: string \"not a number\", \
         expected u8 at line 1 column 14",
        "the interpolated ProtocolError is not repeated, and the serde cause is not dropped"
    );
}

#[test]
fn a_refusal_is_found_through_the_transparent_leg_error() {
    let error = refused();
    assert_eq!(
        broker_refusal(&error),
        Some((
            "unauthenticated".into(),
            "attestation refused: no attestor authority for this subject".into()
        ))
    );
    assert_eq!(broker_refusal(&undecodable()), None);
}

#[test]
fn the_report_is_self_contained() {
    let report = ErrorReport::from_error("open broker session", &refused())
        .with("subject", "discord.1")
        .with("agent", "ville-github")
        .with_note("look in the broker log");
    let text = report.to_string();
    for expected in [
        "dekopon-console: open broker session failed",
        "subject: discord.1",
        "agent: ville-github",
        "error: could not open a broker session for this subject and agent: broker returned \
         unauthenticated: attestation refused: no attestor authority for this subject",
        "broker refusal code: unauthenticated",
        "broker refusal message: attestation refused: no attestor authority for this subject",
        "note: look in the broker log",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
    assert!(report.summary().contains("unauthenticated"));
    assert!(report.summary().contains("e: full error"));
}
