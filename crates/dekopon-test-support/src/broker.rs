use std::{
    collections::VecDeque,
    fs,
    os::{
        fd::{AsFd as _, OwnedFd},
        unix::fs::PermissionsExt as _,
    },
    path::{Path, PathBuf},
};

use dekopon_broker_protocol::{DescriptorStream, FrameLimits, RequestEnvelope, ResponseEnvelope};
use tokio::{net::UnixListener, sync::mpsc, task::JoinHandle};

use crate::FIXTURE_TIMEOUT;

pub type Reply = (ResponseEnvelope, Vec<OwnedFd>);

pub struct BrokerFixture {
    socket: PathBuf,
    requests: mpsc::UnboundedReceiver<RequestEnvelope>,
    server: JoinHandle<()>,
}

impl BrokerFixture {
    #[must_use]
    pub fn answering(
        directory: &Path,
        responses: impl IntoIterator<Item = ResponseEnvelope>,
    ) -> Self {
        let mut responses = responses.into_iter().collect::<VecDeque<_>>();
        Self::scripted(directory, move |_, _| {
            responses.pop_front().map(|response| (response, Vec::new()))
        })
    }

    #[must_use]
    #[allow(
        clippy::let_underscore_must_use,
        reason = "the request log and the reply socket are test-owned; a test that stopped \
                  listening fails on its own missing request or response"
    )]
    pub fn scripted(
        directory: &Path,
        mut script: impl FnMut(&RequestEnvelope, Vec<OwnedFd>) -> Option<Reply> + Send + 'static,
    ) -> Self {
        let socket = directory.join("broker.sock");
        let listener = UnixListener::bind(&socket).expect("bind the fixture broker");
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
            .expect("an owner-only fixture socket");
        let (record, requests) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut stream = DescriptorStream::new(stream);
                let Ok((request, descriptors)) = stream
                    .read_frame::<RequestEnvelope>(FrameLimits::default())
                    .await
                else {
                    continue;
                };
                let reply = script(&request, descriptors);
                let _ = record.send(request);
                let Some((response, descriptors)) = reply else {
                    return;
                };
                let passed = descriptors.iter().map(|fd| fd.as_fd()).collect::<Vec<_>>();
                let _ = stream
                    .write_frame(&response, &passed, FrameLimits::default())
                    .await;
            }
        });
        Self {
            socket,
            requests,
            server,
        }
    }

    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub async fn request(&mut self) -> RequestEnvelope {
        tokio::time::timeout(FIXTURE_TIMEOUT, self.requests.recv())
            .await
            .ok()
            .flatten()
            .expect("the fixture broker recorded a request")
    }

    #[must_use]
    pub fn recorded(&mut self) -> Vec<RequestEnvelope> {
        std::iter::from_fn(|| self.requests.try_recv().ok()).collect()
    }
}

impl Drop for BrokerFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::BrokerFixture;
    use dekopon_broker_protocol::{
        BrokerClient, BrokerRequest, FrameLimits, RequestEnvelope, ResponseEnvelope,
    };
    use std::{
        collections::BTreeMap,
        os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    };

    #[tokio::test]
    async fn the_fixture_answers_the_real_client_in_order_and_records_the_call_past_its_script() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut broker = BrokerFixture::answering(
            directory.path(),
            [ResponseEnvelope::capabilities(
                Vec::new(),
                vec!["probe".to_owned()],
                BTreeMap::new(),
            )],
        );
        let owner = std::fs::metadata(directory.path()).unwrap().uid();
        let client = BrokerClient::new(broker.socket(), owner, FrameLimits::default()).unwrap();
        assert_eq!(
            client.session_surface(None).await.unwrap().command_words,
            ["probe"]
        );
        assert_eq!(broker.request().await, RequestEnvelope::capabilities(None));
        assert!(client.capabilities().await.is_err());
        assert!(matches!(
            &broker.recorded()[..],
            [RequestEnvelope {
                request: BrokerRequest::Capabilities { attestation: None },
                ..
            }]
        ));
    }
}
