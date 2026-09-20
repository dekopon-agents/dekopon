use std::{
    io::IoSlice,
    mem::MaybeUninit,
    os::fd::{AsFd as _, OwnedFd},
};

use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags, sendmsg};
use tokio::{io::Interest, net::UnixStream};

use crate::{
    AssetEncoding, AssetRow, DescriptorStream, FrameLimits, MAX_ASSET_ROWS,
    MAX_DESCRIPTORS_PER_FRAME, NewAsset, ProtocolError, RequestEnvelope, ResponseEnvelope,
    validate_response_descriptors,
};

fn files(count: usize) -> Vec<std::fs::File> {
    (0..count)
        .map(|_| std::fs::File::open("/dev/null").expect("open fixture"))
        .collect()
}

async fn raw_frame(stream: &UnixStream, bytes: &[u8], files: &[std::fs::File]) {
    let descriptors: Vec<_> = files.iter().map(|file| file.as_fd()).collect();
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(200))];
    loop {
        stream.writable().await.expect("writable fixture");
        let mut ancillary = SendAncillaryBuffer::new(&mut space);
        assert!(ancillary.push(SendAncillaryMessage::ScmRights(&descriptors)));
        match stream.try_io(Interest::WRITABLE, || {
            sendmsg(
                stream,
                &[IoSlice::new(bytes)],
                &mut ancillary,
                SendFlags::empty(),
            )
            .map_err(std::io::Error::from)
        }) {
            Ok(count) => {
                assert_eq!(count, bytes.len());
                return;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("fixture sendmsg: {error}"),
        }
    }
}

#[tokio::test]
async fn invoke_round_trips_five_close_on_exec_descriptors() {
    let (writer, reader) = UnixStream::pair().expect("socket pair");
    let mut writer = DescriptorStream::new(writer);
    let mut reader = DescriptorStream::new(reader);
    let directory = tempfile::tempdir().expect("fixture directory");
    let files: Vec<_> = (0..MAX_DESCRIPTORS_PER_FRAME)
        .map(|index| {
            let path = directory.path().join(index.to_string());
            std::fs::write(&path, index.to_string()).expect("fixture bytes");
            std::fs::File::open(path).expect("read-only fixture")
        })
        .collect();
    let descriptors: Vec<_> = files.iter().map(|file| file.as_fd()).collect();
    let request = RequestEnvelope::invoke(None, super::invocation(), vec![], 4);
    writer
        .write_frame(&request, &descriptors, FrameLimits::default())
        .await
        .expect("write frame");
    let (actual, received): (RequestEnvelope, Vec<OwnedFd>) = reader
        .read_frame(FrameLimits::default())
        .await
        .expect("read frame");
    assert_eq!(actual, request);
    assert_eq!(received.len(), MAX_DESCRIPTORS_PER_FRAME);
    for (index, descriptor) in received.into_iter().enumerate() {
        assert!(
            rustix::io::fcntl_getfd(&descriptor)
                .expect("descriptor flags")
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        let file = std::fs::File::from(descriptor);
        let mut bytes = [0];
        std::os::unix::fs::FileExt::read_exact_at(&file, &mut bytes, 0).expect("positional read");
        assert_eq!(&bytes, index.to_string().as_bytes());
        assert!(std::os::unix::fs::FileExt::write_at(&file, b"x", 0).is_err());
    }
}

#[test]
fn sixteen_descriptors_are_refused_and_every_one_is_closed() {
    // Linux counts run alone so unrelated tests cannot change the process-wide total.
    #[cfg(target_os = "linux")]
    {
        const CHILD: &str = "DEKOPON_SIXTEEN_DESCRIPTORS_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(
                std::env::current_exe().expect("test executable"),
            )
            .args([
                "--exact",
                "tests::asset_descriptors::sixteen_descriptors_are_refused_and_every_one_is_closed",
            ])
            .env(CHILD, "1")
            .status()
            .expect("isolated descriptor count test");
            assert!(status.success());
            return;
        }
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let (writer, reader) = UnixStream::pair().expect("socket pair");
            let mut reader = DescriptorStream::new(reader);
            let files = files(16);
            #[cfg(target_os = "linux")]
            let count = || {
                std::fs::read_dir("/proc/self/fd")
                    .expect("fd directory")
                    .count()
            };
            #[cfg(target_os = "linux")]
            let before = count();
            raw_frame(&writer, &[0, 0, 0, 2, b'{', b'}'], &files).await;
            assert!(matches!(
                reader
                    .read_frame::<serde_json::Value>(FrameLimits::default())
                    .await,
                Err(ProtocolError::TooManyDescriptors)
            ));
            #[cfg(target_os = "linux")]
            assert_eq!(count(), before);
            // Received raw descriptors are not exposed on refusal; macOS checks the error only.
        });
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn two_hundred_descriptors_arrive_untruncated_on_linux() {
    let (writer, reader) = UnixStream::pair().expect("socket pair");
    let mut reader = DescriptorStream::new(reader);
    reader.descriptor_limit = 200;
    let files = files(200);
    raw_frame(&writer, &[0, 0, 0, 2, b'{', b'}'], &files).await;
    let (value, received) = reader
        .read_frame::<serde_json::Value>(FrameLimits::default())
        .await
        .expect("all descriptors arrive without CTRUNC");
    assert_eq!(value, serde_json::json!({}));
    assert_eq!(received.len(), 200);
}

#[tokio::test]
async fn too_many_outgoing_descriptors_are_refused_before_writing() {
    let (writer, _reader) = UnixStream::pair().expect("socket pair");
    let mut writer = DescriptorStream::new(writer);
    let files = files(MAX_DESCRIPTORS_PER_FRAME + 1);
    let descriptors: Vec<_> = files.iter().map(|file| file.as_fd()).collect();
    assert!(matches!(
        writer
            .write_frame(&(), &descriptors, FrameLimits::default())
            .await,
        Err(ProtocolError::TooManyDescriptors)
    ));
}

#[tokio::test]
async fn descriptor_cap_applies_across_all_reads_of_one_frame() {
    let (writer, reader) = UnixStream::pair().expect("socket pair");
    let mut reader = DescriptorStream::new(reader);
    let files = files(3);
    raw_frame(&writer, &[0, 0, 0, 2], &files).await;
    raw_frame(&writer, b"{}", &files).await;
    assert!(matches!(
        reader
            .read_frame::<serde_json::Value>(FrameLimits::default())
            .await,
        Err(ProtocolError::TooManyDescriptors)
    ));
}

#[test]
fn asset_metadata_is_strict_and_sends_remaining_is_camel_case() {
    let request = RequestEnvelope::invoke(None, super::invocation(), vec![], 4);
    let value = serde_json::to_value(request).expect("request JSON");
    assert_eq!(value["request"]["sendsRemaining"], 4);
    let mut row = serde_json::json!({
        "id": 1, "contentType": "image/png", "encoding": "identity",
        "bytes": 0, "origin": "chat", "sent": false
    });
    serde_json::from_value::<AssetRow>(row.clone()).expect("typed row");
    row["authority"] = true.into();
    assert!(serde_json::from_value::<AssetRow>(row).is_err());
    let mut output = serde_json::json!({
        "descriptor": 0, "contentType": "image/png", "encoding": "base64",
        "bytes": 0, "sha256": "0".repeat(64)
    });
    serde_json::from_value::<NewAsset>(output.clone()).expect("typed output");
    output["authority"] = true.into();
    assert!(serde_json::from_value::<NewAsset>(output).is_err());
}

#[test]
fn typed_rows_and_response_indexes_are_exact() {
    let row = AssetRow {
        id: 1,
        content_type: "image/png".into(),
        encoding: AssetEncoding::Identity,
        bytes: 0,
        origin: "chat".into(),
        sent: false,
    };
    let request = RequestEnvelope::invoke(
        None,
        super::invocation(),
        vec![row.clone(); MAX_ASSET_ROWS],
        4,
    );
    request.request.validate().expect("row cap inclusive");
    let request =
        RequestEnvelope::invoke(None, super::invocation(), vec![row; MAX_ASSET_ROWS + 1], 4);
    assert!(matches!(
        request.request.validate(),
        Err(ProtocolError::TooManyAssetRows)
    ));
    let asset = NewAsset {
        descriptor: 0,
        content_type: "image/png".into(),
        encoding: AssetEncoding::Identity,
        bytes: 0,
        sha256: "0".repeat(64),
    };
    let response = ResponseEnvelope::invocation(
        super::failed_with_detail(),
        vec![asset.clone()],
        vec![],
        vec![],
    );
    validate_response_descriptors(&response.response, 1).expect("exact descriptor index");
    for count in [0, 2] {
        assert!(matches!(
            validate_response_descriptors(&response.response, count),
            Err(ProtocolError::DescriptorIndex)
        ));
    }
    let response = ResponseEnvelope::invocation(
        super::failed_with_detail(),
        vec![asset.clone(), asset.clone()],
        vec![],
        vec![],
    );
    assert!(matches!(
        validate_response_descriptors(&response.response, 2),
        Err(ProtocolError::DescriptorIndex)
    ));
    let mut wrong = asset;
    wrong.descriptor = 1;
    let response =
        ResponseEnvelope::invocation(super::failed_with_detail(), vec![wrong], vec![], vec![]);
    assert!(matches!(
        validate_response_descriptors(&response.response, 1),
        Err(ProtocolError::DescriptorIndex)
    ));
    let response = ResponseEnvelope::capabilities(vec![], vec![]);
    assert!(matches!(
        validate_response_descriptors(&response.response, 1),
        Err(ProtocolError::UnexpectedDescriptors)
    ));
}

// The child runs alone: process-wide fd counts must not race unrelated parallel tests.
#[cfg(target_os = "linux")]
#[test]
fn rejected_frames_close_every_received_descriptor() {
    const CHILD: &str = "DEKOPON_DESCRIPTOR_COUNT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "tests::asset_descriptors::rejected_frames_close_every_received_descriptor",
            ])
            .env(CHILD, "1")
            .status()
            .expect("isolated descriptor count test");
        assert!(status.success());
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let (writer, reader) = UnixStream::pair().expect("socket pair");
            let mut reader = DescriptorStream::new(reader);
            let files = files(5);
            let count = || {
                std::fs::read_dir("/proc/self/fd")
                    .expect("fd directory")
                    .count()
            };
            let before = count();
            raw_frame(&writer, &[0, 0, 0, 1, b'!'], &files).await;
            assert!(matches!(
                reader
                    .read_frame::<RequestEnvelope>(FrameLimits::default())
                    .await,
                Err(ProtocolError::Deserialize { .. })
            ));
            assert_eq!(count(), before);
            raw_frame(&writer, &[0, 0, 0, 0], &files).await;
            assert!(matches!(
                reader
                    .read_frame::<RequestEnvelope>(FrameLimits::default())
                    .await,
                Err(ProtocolError::EmptyFrame)
            ));
            assert_eq!(count(), before);
            raw_frame(&writer, &u32::MAX.to_be_bytes(), &files).await;
            assert!(matches!(
                reader
                    .read_frame::<RequestEnvelope>(FrameLimits::default())
                    .await,
                Err(ProtocolError::FrameTooLarge { .. })
            ));
            assert_eq!(count(), before);
            raw_frame(&writer, &[0, 0, 0, 2], &self::files(16)).await;
            assert!(matches!(
                reader
                    .read_frame::<RequestEnvelope>(FrameLimits::default())
                    .await,
                Err(ProtocolError::TooManyDescriptors)
            ));
            assert_eq!(count(), before);
            let response = ResponseEnvelope::capabilities(vec![], vec![]);
            let mut writer = DescriptorStream::new(writer);
            let descriptors: Vec<_> = files.iter().map(|file| file.as_fd()).collect();
            writer
                .write_frame(&response, &descriptors, FrameLimits::default())
                .await
                .expect("write frame");
            let (response, received): (ResponseEnvelope, _) = reader
                .read_frame(FrameLimits::default())
                .await
                .expect("typed frame");
            assert!(matches!(
                validate_response_descriptors(&response.response, received.len()),
                Err(ProtocolError::UnexpectedDescriptors)
            ));
            drop(received);
            assert_eq!(count(), before);
        });
}
