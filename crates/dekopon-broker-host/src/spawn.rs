use wasmtime::component::Resource;

use crate::{StoreState, bindings::dekopon::spawn::run as wit};

#[derive(Debug, thiserror::Error)]
pub(crate) enum SpawnTrap {
    #[error("this broker cannot run a child script")]
    Unavailable,
}

impl wit::Host for StoreState {
    async fn run(
        &mut self,
        _script: String,
        _stdin: wit::Stdin,
    ) -> wasmtime::Result<Result<wit::Child, wit::SpawnError>> {
        Err(SpawnTrap::Unavailable.into())
    }
}

impl wit::HostStatus for StoreState {
    async fn wait(&mut self, _status: Resource<wit::Status>) -> wasmtime::Result<wit::Exit> {
        Err(SpawnTrap::Unavailable.into())
    }

    async fn drop(&mut self, _status: Resource<wit::Status>) -> wasmtime::Result<()> {
        Err(SpawnTrap::Unavailable.into())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{
        BrokerHostLimits, BrokerHostOptions, Runtime, clock::ClockState, http::HttpState,
        settings::SettingsState, storage::StorageState,
    };

    use super::{SpawnTrap, wit};

    #[tokio::test]
    async fn spawn_refusal_is_typed_and_does_not_poison_the_host() {
        use wit::Host as _;

        let runtime = Runtime::new(BrokerHostLimits::default(), &BrokerHostOptions::default())
            .expect("runtime");
        let http = HttpState::describe(runtime.http_ceilings(), Duration::from_secs(5))
            .expect("disabled HTTP");
        let mut store = runtime
            .store_for_provider(
                "test-provider",
                http,
                StorageState::disabled(),
                ClockState::invoke(None),
                SettingsState::describe(),
            )
            .expect("store");
        for _ in 0..2 {
            let error = store
                .data_mut()
                .run("echo child".to_owned(), wit::Stdin::None)
                .await
                .expect_err("staged host refuses child");
            assert!(error.downcast_ref::<SpawnTrap>().is_some());
        }
    }
}
