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
