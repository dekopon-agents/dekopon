//! The unprivileged runtime for configured Dekopon agents.
//!
//! [`session::SessionEngine`] owns the bounded model/tool loop. [`bootstrap::SessionBootstrap`]
//! supplies its request context, while [`runtime`] adapts import-free direct execution and the
//! authenticated broker client. Capability metadata is model-visible data, never authority.
//! Only the separate broker authorizes provider effects; this crate has no policy or credentials.

#![forbid(unsafe_code)]

pub mod accounting;
pub mod activity;
pub mod bootstrap;
pub mod checkpoint;
pub mod context;
pub mod control;
pub mod conversation;
pub mod history;
pub mod improvement;
pub mod meta;
pub mod replay;
pub mod runtime;
pub mod session;
pub mod skills;
pub mod tools;
