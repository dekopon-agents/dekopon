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

#[tokio::test]
async fn a_slice_read_failure_retains_its_io_kind_through_http() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let _closed = socket.read_to_end(&mut bytes).await;
    });
    let file = Arc::new(tempfile::tempfile().unwrap());
    let body = SliceBody::new(&[Chunk::File {
        file,
        offset: 0,
        len: 1,
    }])
    .unwrap();
    let error = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{address}"))
        .body(reqwest::Body::wrap(body))
        .send()
        .await
        .unwrap_err();
    let error = crate::http::http_failure(crate::error::FailurePhase::AwaitingHeaders, None, error);
    let crate::error::InferenceError::Transport(crate::error::TransportFailure::Body(error)) =
        error
    else {
        panic!("typed body failure: {error:?}");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    server.await.unwrap();
}

#[tokio::test]
async fn joined_text_splices_multiple_file_slices_inside_one_json_string() {
    let mut first = tempfile::tempfile().unwrap();
    first.write_all(b"first\\n").unwrap();
    let mut second = tempfile::tempfile().unwrap();
    second.write_all("secondé".as_bytes()).unwrap();
    let texts = [
        crate::text::Text::Owned("Source:\n".to_owned()),
        crate::text::Text::File {
            file: Arc::new(first),
            offset: 0,
            len: 7,
        },
        crate::text::Text::File {
            file: Arc::new(second),
            offset: 0,
            len: 8,
        },
    ];
    let mut encoder = super::JsonBody::default();
    encoder.joined(&texts).unwrap();
    let chunks = encoder.finish();
    assert_eq!(
        chunks
            .iter()
            .filter(|chunk| matches!(chunk, Chunk::File { .. }))
            .count(),
        2
    );
    let encoded = super::collect(&chunks).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<String>(&encoded).unwrap(),
        "Source:\nfirst\nsecondé"
    );
}
