//! Terminal console for driving Dekopon agents against a running local broker.
//!
//! The console is the gateway half of a development deployment, and nothing more privileged than
//! that. `dekopon-brokerd` holds the policy, the provider credentials, and the components; it has
//! no model client and no concept of a turn, so something has to run the loop. In production that
//! is `dekopond`, woken by a chat transport. Here it is this crate, woken by somebody typing.
//!
//! What that buys is the whole reason it exists: **tool-call arguments and results exist only in
//! the process running the loop.** `dekopon_agent::prompt::History` keeps the prompt and the answer
//! and drops every tool call before recording a turn; `shell.command` spans carry an argument count
//! and never argument values; the broker's audit chain carries digests, never payloads. Wrapping
//! the two seams the loop is built on — [`record::RecordingRuntime`] and [`record::RecordingInvoker`]
//! — is the only place those values can be observed, and observing them costs no change anywhere
//! else in the workspace.
//!
//! It holds a model credential and nothing else. Every capability call is proposed to the broker on
//! behalf of an attested subject, and the broker alone decides it.

#![forbid(unsafe_code)]
#![cfg(unix)]

pub mod app;
pub mod record;
pub mod redact;
pub mod run;
pub mod session;
pub mod transcript;
pub mod ui;

pub use app::{App, Mode, Notice, Pane, ShellEntry};
pub use run::{ConsoleExit, run};
pub use session::{
    AgentSession, CONSOLE_AUTH_FILE_NAME, ConsoleOptions, ModelChoice, SessionError, StopFlag,
    TRACE_PREFIX,
};
