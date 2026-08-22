//! Bounded model transports and model-account authentication for Dekopon.
//!
//! This crate owns credentials used to authenticate model endpoints. Provider credentials remain
//! a separate concern and are never exposed through these clients.

#![forbid(unsafe_code)]

/// Native ChatGPT/Codex subscription authentication and Responses transport.
pub mod chatgpt;
#[cfg(test)]
mod mock;
/// Bounded generated-image clients and output types.
pub mod image;
/// Generic chat-model contract and OpenAI-compatible transport.
pub mod model;
