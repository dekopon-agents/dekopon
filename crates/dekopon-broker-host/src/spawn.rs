use std::os::fd::{AsFd as _, OwnedFd};

use dekopon_broker_protocol::{UpcallStdin, UpcallStreams};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::Resource;

use crate::stdio::ReaderResource;
use crate::{StoreState, bindings::dekopon::spawn::run as wit};

const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SpawnTrap {
    #[error("this store has no gateway conversation to run a child script")]
    Unavailable,
    #[error("the child script is larger than the invocation input limit")]
    ScriptTooLarge,
    #[error("the gateway conversation ended before the child exited")]
    ConversationClosed,
    #[error("could not create the child's streams")]
    Streams { kind: std::io::ErrorKind },
}

/// A child script for the invocation's connection to send to the gateway; dropping it closes its
/// descriptors and tells the waiting guest the conversation ended.
#[derive(Debug)]
pub struct UpcallRequest {
    pub script: String,
    pub stdin: UpcallStdin,
    pub streams: UpcallStreams,
    pub reply: oneshot::Sender<UpcallExit>,
}

#[derive(Debug)]
pub struct UpcallExit {
    pub status: u8,
    pub stderr: String,
}

pub struct StatusResource;

#[derive(Debug, Default)]
pub(crate) struct SpawnState {
    upcalls: Option<mpsc::Sender<UpcallRequest>>,
    max_script_bytes: usize,
    generation: u64,
    child: Option<Child>,
}

#[derive(Debug)]
struct Child {
    generation: u64,
    stdout: Option<UnixStream>,
    exit: oneshot::Receiver<UpcallExit>,
    pump: Option<tokio::task::AbortHandle>,
}

impl Drop for Child {
    fn drop(&mut self) {
        if let Some(pump) = &self.pump {
            pump.abort();
        }
    }
}

impl SpawnState {
    pub(crate) fn invoke(
        upcalls: Option<mpsc::Sender<UpcallRequest>>,
        max_script_bytes: usize,
    ) -> Self {
        Self {
            upcalls,
            max_script_bytes,
            ..Self::default()
        }
    }
}

fn streams_failed(error: std::io::Error) -> SpawnTrap {
    SpawnTrap::Streams { kind: error.kind() }
}

fn socket_pair() -> Result<(UnixStream, OwnedFd), SpawnTrap> {
    let (host, peer) = std::os::unix::net::UnixStream::pair().map_err(streams_failed)?;
    host.set_nonblocking(true).map_err(streams_failed)?;
    let host = UnixStream::from_std(host).map_err(streams_failed)?;
    Ok((host, peer.into()))
}

async fn pump(mut source: UnixStream, mut sink: UnixStream) {
    let mut buffer = vec![0; CHUNK_BYTES];
    while let Ok(count) = source.read(&mut buffer).await {
        if count == 0 || sink.write_all(&buffer[..count]).await.is_err() {
            break;
        }
    }
}

type ChildStdin = (
    UpcallStdin,
    Option<OwnedFd>,
    Option<(UnixStream, UnixStream)>,
);

impl StoreState {
    fn child_stdin(&mut self, stdin: wit::Stdin) -> wasmtime::Result<ChildStdin> {
        match stdin {
            wit::Stdin::None => Ok((UpcallStdin::None, None, None)),
            wit::Stdin::Inherit => match &self.stdio.stdin {
                Some(stream) => {
                    let shared = stream
                        .as_fd()
                        .try_clone_to_owned()
                        .map_err(streams_failed)?;
                    Ok((UpcallStdin::Inherit, Some(shared), None))
                }
                None => Ok((UpcallStdin::None, None, None)),
            },
            wit::Stdin::Reader(reader) => {
                let source = match self.table.delete(reader)? {
                    ReaderResource::Stdin => self.stdio.stdin.take(),
                    ReaderResource::Child(_) => None,
                };
                self.stdio.release();
                let (sink, read) = socket_pair()?;
                Ok((
                    UpcallStdin::Reader,
                    Some(read),
                    source.map(|source| (source, sink)),
                ))
            }
        }
    }

    pub(crate) async fn read_child(&mut self, generation: u64, length: usize) -> Vec<u8> {
        let Some(child) = self
            .spawn
            .child
            .as_mut()
            .filter(|child| child.generation == generation)
        else {
            return Vec::new();
        };
        let Some(stream) = child.stdout.as_mut() else {
            return Vec::new();
        };
        let mut buffer = vec![0; length];
        let parked = self.stdio.clock.park();
        let read = stream.read(&mut buffer).await;
        drop(parked);
        match read {
            Ok(count) => {
                buffer.truncate(count);
                if count == 0 {
                    child.stdout = None;
                }
                buffer
            }
            Err(_) => {
                child.stdout = None;
                Vec::new()
            }
        }
    }
}

impl wit::Host for StoreState {
    async fn run(
        &mut self,
        script: String,
        stdin: wit::Stdin,
    ) -> wasmtime::Result<Result<wit::Child, wit::SpawnError>> {
        if self.spawn.upcalls.is_none() {
            return Err(SpawnTrap::Unavailable.into());
        }
        if self.spawn.child.is_some() {
            if let wit::Stdin::Reader(reader) = stdin {
                self.table.delete(reader)?;
                self.stdio.release();
            }
            return Ok(Err(wit::SpawnError::Busy));
        }
        if script.len() > self.spawn.max_script_bytes {
            return Err(SpawnTrap::ScriptTooLarge.into());
        }
        self.stdio.mint()?;
        self.stdio.mint()?;
        let (stdout, child_stdout) = socket_pair()?;
        let (kind, child_stdin, pumped) = self.child_stdin(stdin)?;
        let (reply, exit) = oneshot::channel();
        let request = UpcallRequest {
            script,
            stdin: kind,
            streams: UpcallStreams {
                stdout: child_stdout,
                stdin: child_stdin,
            },
            reply,
        };
        let parked = self.stdio.clock.park();
        let sent = match &self.spawn.upcalls {
            Some(upcalls) => upcalls.send(request).await.is_ok(),
            None => false,
        };
        drop(parked);
        if !sent {
            return Err(SpawnTrap::ConversationClosed.into());
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "owner: the child slot aborts it when the child is waited, abandoned or the store ends; bound: one pump per child, one child per invocation"
        )]
        let pump = pumped.map(|(source, sink)| tokio::spawn(pump(source, sink)).abort_handle());
        self.spawn.generation += 1;
        let generation = self.spawn.generation;
        self.spawn.child = Some(Child {
            generation,
            stdout: Some(stdout),
            exit,
            pump,
        });
        Ok(Ok(wit::Child {
            stdout: self.table.push(ReaderResource::Child(generation))?,
            status: self.table.push(StatusResource)?,
        }))
    }
}

impl wit::HostStatus for StoreState {
    async fn wait(&mut self, status: Resource<StatusResource>) -> wasmtime::Result<wit::Exit> {
        self.table.delete(status)?;
        self.stdio.release();
        let mut child = self
            .spawn
            .child
            .take()
            .ok_or(SpawnTrap::ConversationClosed)?;
        let parked = self.stdio.clock.park();
        // The gateway writes the child's whole output before its status, so a guest that waits
        // before reading would deadlock against a full socket unless the host drains it first.
        if let Some(mut stdout) = child.stdout.take() {
            let mut discarded = vec![0; CHUNK_BYTES];
            while matches!(stdout.read(&mut discarded).await, Ok(count) if count > 0) {}
        }
        let exit = (&mut child.exit).await;
        drop(parked);
        let exit = exit.map_err(|_closed| SpawnTrap::ConversationClosed)?;
        Ok(wit::Exit {
            status: exit.status,
            stderr: exit.stderr,
        })
    }

    async fn drop(&mut self, status: Resource<StatusResource>) -> wasmtime::Result<()> {
        self.table.delete(status)?;
        self.stdio.release();
        self.spawn.child = None;
        Ok(())
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
