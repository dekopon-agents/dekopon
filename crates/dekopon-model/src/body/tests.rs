use std::{io::Write, sync::Arc};

use bytes::Bytes;
use futures_util::future::poll_fn;
use http_body::Body;

use super::{BUFFER_BYTES, Chunk, OpaqueSource, SliceBody};

async fn drain(body: &mut SliceBody) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(frame) = poll_fn(|cx| std::pin::Pin::new(&mut *body).poll_frame(cx)).await {
        let frame = frame?;
        let data = frame.into_data().expect("data frame");
        assert!(data.len() <= BUFFER_BYTES);
        let before = body.size_hint().exact().unwrap();
        bytes.extend_from_slice(&data);
        assert_eq!(before + bytes.len() as u64, 2 * BUFFER_BYTES as u64 + 33);
    }
    Ok(bytes)
}

#[tokio::test]
async fn slice_body_rebuilds_on_the_held_inode_with_an_exact_length() {
    let mut file = tempfile::tempfile().unwrap();
    let payload = vec![b'x'; 2 * BUFFER_BYTES + 30];
    file.write_all(b"prefix").unwrap();
    file.write_all(&payload).unwrap();
    let file = Arc::new(file);
    let chunks = vec![
        Chunk::Literal(Bytes::from_static(b"[\"")),
        Chunk::File {
            file: Arc::clone(&file),
            offset: 6,
            len: payload.len() as u64,
        },
        Chunk::Literal(Bytes::from_static(b"\"")),
    ];
    let mut first = SliceBody::new(&chunks).unwrap();
    let mut second = SliceBody::new(&chunks).unwrap();
    let bytes = drain(&mut first).await.unwrap();
    assert_eq!(bytes, drain(&mut second).await.unwrap());
    assert_eq!(&bytes[2..bytes.len() - 1], payload);
    assert!(first.is_end_stream());
}

#[tokio::test]
async fn an_opaque_slice_is_unquoted_and_a_short_read_fails_once() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(b"{\"reasoning\":1}").unwrap();
    let chunks = [Chunk::Opaque(OpaqueSource::File {
        file: Arc::new(file),
        offset: 0,
        len: 15,
    })];
    let mut body = SliceBody::new(&chunks).unwrap();
    let frame = poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.into_data().unwrap(), b"{\"reasoning\":1}".as_slice());
    let file = Arc::new(tempfile::tempfile().unwrap());
    let mut body = SliceBody::new(&[Chunk::File {
        file,
        offset: 0,
        len: 1,
    }])
    .unwrap();
    let error = poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    assert!(
        poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx))
            .await
            .is_none()
    );
}
