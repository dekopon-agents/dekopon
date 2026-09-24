use crate::StoreState;
use crate::bindings::dekopon::settings::config;

#[derive(Debug)]
pub(crate) enum SettingsState {
    Invoke(Option<String>),
    Refused { attempted: bool },
}

impl SettingsState {
    pub(crate) const fn describe() -> Self {
        Self::Refused { attempted: false }
    }

    pub(crate) fn invoke(settings: Option<String>) -> Self {
        Self::Invoke(settings)
    }

    pub(crate) const fn attempted(&self) -> bool {
        matches!(self, Self::Refused { attempted: true })
    }
}

impl config::Host for StoreState {
    async fn get(&mut self) -> wasmtime::Result<Option<String>> {
        match &mut self.settings {
            SettingsState::Invoke(settings) => Ok(settings.clone()),
            SettingsState::Refused { attempted } => {
                *attempted = true;
                Err(wasmtime::Error::msg(
                    "provider read dekopon:settings/config@0.1.0 outside invoke",
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SettingsState;

    #[test]
    fn only_out_of_invoke_access_trips_the_refusal() {
        assert!(!SettingsState::describe().attempted());
        assert!(!SettingsState::invoke(None).attempted());
        assert!(!SettingsState::invoke(Some("{}".to_owned())).attempted());
        assert!(SettingsState::Refused { attempted: true }.attempted());
    }
}
