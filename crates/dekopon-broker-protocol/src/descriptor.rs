use std::{
    io::{self, IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::fd::{AsFd as _, BorrowedFd, OwnedFd},
    pin::Pin,
    task::{Context, Poll, ready},
};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncWrite, Interest, ReadBuf},
    net::UnixStream,
};

use crate::{FrameLimits, MAX_DESCRIPTORS_PER_FRAME, ProtocolError, read_frame, write_frame};

/// Owned broker socket. Every read, including the frame prefix, receives ancillary data.
/// Typed callers reject descriptors on operations other than Invoke/Invocation.
pub struct DescriptorStream {
    stream: UnixStream,
    received: Vec<OwnedFd>,
    sending: Vec<OwnedFd>,
    receive_error: Option<ProtocolError>,
}

impl DescriptorStream {
    /// Wraps either end of a broker connection.
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            received: Vec::new(),
            sending: Vec::new(),
            receive_error: None,
        }
    }

    /// Reads a frame and transfers its descriptors to the typed caller for validation.
    pub async fn read_frame<T: DeserializeOwned>(
        &mut self,
        limits: FrameLimits,
    ) -> Result<(T, Vec<OwnedFd>), ProtocolError> {
        let result = read_frame(self, limits).await;
        if let Some(error) = self.receive_error.take() {
            self.received.clear();
            return Err(error);
        }
        match result {
            Ok(value) => Ok((value, std::mem::take(&mut self.received))),
            Err(error) => {
                self.received.clear();
                Err(error)
            }
        }
    }

    /// Attaches descriptors to the sendmsg carrying only the frame's first byte.
    pub async fn write_frame<T: Serialize>(
        &mut self,
        value: &T,
        descriptors: &[BorrowedFd<'_>],
        limits: FrameLimits,
    ) -> Result<(), ProtocolError> {
        if descriptors.len() > MAX_DESCRIPTORS_PER_FRAME {
            return Err(ProtocolError::TooManyDescriptors);
        }
        self.sending = descriptors
            .iter()
            .map(|fd| fd.try_clone_to_owned())
            .collect::<io::Result<Vec<_>>>()
            .map_err(|source| ProtocolError::Io { source })?;
        let result = write_frame(self, value, limits).await;
        self.sending.clear();
        result
    }

    fn receive(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let mut space =
            [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_DESCRIPTORS_PER_FRAME))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        #[cfg(target_os = "linux")]
        let flags = RecvFlags::CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = RecvFlags::empty();
        let message = self.stream.try_io(Interest::READABLE, || {
            recvmsg(&self.stream, &mut [IoSliceMut::new(bytes)], &mut ancillary, flags)
                .map_err(io::Error::from)
        })?;
        let mut descriptors = Vec::new();
        for message in ancillary.drain() {
            if let RecvAncillaryMessage::ScmRights(rights) = message {
                descriptors.extend(rights);
            }
        }
        // macOS lacks MSG_CMSG_CLOEXEC; secure every received descriptor before exposing it.
        #[cfg(not(target_os = "linux"))]
        for descriptor in &descriptors {
            if let Err(source) = rustix::io::fcntl_setfd(descriptor, rustix::io::FdFlags::CLOEXEC) {
                self.receive_error = Some(ProtocolError::DescriptorFlags {
                    source: source.into(),
                });
                return Err(io::Error::other("could not secure received descriptor"));
            }
        }
        let error = if message.flags.contains(ReturnFlags::CTRUNC) {
            Some(ProtocolError::DescriptorsTruncated)
        } else if self.received.len() + descriptors.len() > MAX_DESCRIPTORS_PER_FRAME {
            Some(ProtocolError::TooManyDescriptors)
        } else {
            None
        };
        if let Some(error) = error {
            self.receive_error = Some(error);
            return Err(io::Error::other("invalid broker frame descriptors"));
        }
        self.received.extend(descriptors);
        Ok(message.bytes)
    }
}

impl AsyncRead for DescriptorStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            ready!(this.stream.poll_read_ready(cx))?;
            match this.receive(buffer.initialize_unfilled()) {
                Ok(count) => {
                    buffer.advance(count);
                    return Poll::Ready(Ok(()));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}

impl AsyncWrite for DescriptorStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.sending.is_empty() || bytes.is_empty() {
            return Pin::new(&mut this.stream).poll_write(cx, bytes);
        }
        loop {
            ready!(this.stream.poll_write_ready(cx))?;
            let descriptors: Vec<_> = this.sending.iter().map(|fd| fd.as_fd()).collect();
            let mut space =
                [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_DESCRIPTORS_PER_FRAME))];
            let mut ancillary = SendAncillaryBuffer::new(&mut space);
            if !ancillary.push(SendAncillaryMessage::ScmRights(&descriptors)) {
                return Poll::Ready(Err(io::Error::other(
                    "broker descriptor buffer is too small",
                )));
            }
            match this.stream.try_io(Interest::WRITABLE, || {
                sendmsg(
                    &this.stream,
                    &[IoSlice::new(&bytes[..1])],
                    &mut ancillary,
                    SendFlags::empty(),
                )
                .map_err(io::Error::from)
            }) {
                Ok(count) => {
                    if count != 0 {
                        this.sending.clear();
                    }
                    return Poll::Ready(Ok(count));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}
