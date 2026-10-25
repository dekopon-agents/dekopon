//! Terminal console for driving Dekopon agents against a running local broker.
//!
//! The console is the gateway half of a development deployment, and nothing more privileged than
//! that. `dekopon-brokerd` holds the policy, the provider credentials, and the components; it has
//! no model client and no concept of a turn, so something has to run the loop. In production that
//! is `dekopon-gatewayd`, woken by a chat transport. Here it is this crate, woken by somebody typing.
//!
//! The local transcript records tool arguments/results for interactive inspection. Shared core
//! telemetry separately exports complete prompt/script/provider payloads under one turn trace
//! when configured, excluding model/provider credentials and telemetry authentication headers.
//!
//! It holds a model credential and nothing else. Every capability call is proposed to the broker on
//! behalf of an attested subject, and the broker alone decides it.

#![forbid(unsafe_code)]
#![cfg(unix)]

pub mod app;
pub mod profile;
pub mod record;
pub mod redact;
pub mod report;
pub mod run;
pub mod session;
pub mod structured;
pub mod transcript;
pub mod ui;

pub use app::{App, Mode, Notice, Pane, Payload, RevealedField, ShellEntry};
pub use profile::OperatorProfile;
pub use report::ErrorReport;
pub use run::{ConsoleExit, run};
pub use session::{
    AgentSession, CONSOLE_AUTH_FILE_NAME, ConsoleOptions, ModelChoice, SessionError, StopFlag,
};
