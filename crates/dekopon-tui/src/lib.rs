#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
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
