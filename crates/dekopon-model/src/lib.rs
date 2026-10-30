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
pub mod asset;

pub mod blocking;
pub mod body;
pub mod chatgpt;
pub mod codex;
pub mod control;
mod diagnostic;
pub mod error;
mod http;
pub mod inference;
mod loopback;
#[cfg(test)]
mod mock;
pub mod model;
pub mod openai;
pub mod openrouter;
pub mod replay;
mod sse;
pub mod stream;
pub mod text;
#[cfg(test)]
mod trace_capture;
pub mod transcript;
pub mod wire;

pub use stream::{ModelText, TranscriptDialect, TurnEvent, events_from_transcript};
