use std::{num::NonZeroU32, time::Duration};

use serde::{Deserialize, Deserializer};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReloadSettings {
    #[serde(default)]
    pub mode: ReloadMode,
    #[serde(default, rename = "debounceMs")]
    pub debounce: Debounce,
    #[serde(default, rename = "activationTimeoutMs")]
    pub activation_timeout: ActivationTimeout,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReloadMode {
    #[default]
    Aggressive,
    Signaled,
}

impl<'de> Deserialize<'de> for ReloadMode {
    #[expect(
        clippy::todo,
        unused_variables,
        reason = "skeleton: daemon/watcher-core"
    )]
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        todo!()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(from = "NonZeroU32")]
pub struct Debounce(Duration);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(from = "NonZeroU32")]
pub struct ActivationTimeout(Duration);

#[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
impl Default for Debounce {
    fn default() -> Self {
        todo!()
    }
}

#[expect(clippy::todo, reason = "skeleton: daemon/watcher-core")]
impl Default for ActivationTimeout {
    fn default() -> Self {
        todo!()
    }
}

#[expect(
    clippy::todo,
    unused_variables,
    reason = "skeleton: daemon/watcher-core"
)]
impl From<NonZeroU32> for Debounce {
    fn from(milliseconds: NonZeroU32) -> Self {
        todo!()
    }
}

#[expect(
    clippy::todo,
    unused_variables,
    reason = "skeleton: daemon/watcher-core"
)]
impl From<NonZeroU32> for ActivationTimeout {
    fn from(milliseconds: NonZeroU32) -> Self {
        todo!()
    }
}

impl Debounce {
    pub const fn duration(self) -> Duration {
        self.0
    }
}

impl ActivationTimeout {
    pub const fn duration(self) -> Duration {
        self.0
    }
}
