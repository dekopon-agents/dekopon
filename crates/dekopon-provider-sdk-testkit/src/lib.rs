#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(test, allow(clippy::disallowed_methods, clippy::disallowed_types))]

mod assets;
mod conformance;
mod typed;

pub use conformance::{ConformanceError, conformance};
pub use dekopon_broker_host::BrokerHostLimits;
pub use dekopon_capability::AssetConstraints;
pub use typed::{
    ChildInput, ChildRun, ChildScript, ComponentOutput, Harness, HarnessError, HttpScript, Native,
    NativeOutput, Run,
};
