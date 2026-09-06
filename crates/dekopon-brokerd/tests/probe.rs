//! Black-box health checks never start the daemon or load configuration.
use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{io::AsyncWriteExt as _, net::UnixListener};

async fn probe(socket: &std::path::Path) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dekopon-brokerd"))
        .args(["probe", "--socket"])
        .arg(socket)
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= until {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("probe process exceeded deadline");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    child.wait_with_output().unwrap()
}

#[tokio::test]
async fn absent_refused_wrong_protocol_and_stalled_sockets_fail() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("broker.sock");
    let absent = probe(&socket).await;
    assert_eq!(absent.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&absent.stderr).contains("probe failed"));
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    drop(listener);
    let refused = probe(&socket).await;
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("connect"));
    fs::remove_file(&socket).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let wrong = async {
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
    };
    let (wrong, ()) = tokio::join!(probe(&socket), wrong);
    assert_eq!(wrong.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("probe failed"));
    let (stalled, connection) = tokio::join!(
        probe(&socket),
        tokio::time::timeout(Duration::from_secs(5), listener.accept())
    );
    let connection = connection.unwrap().unwrap();
    assert_eq!(stalled.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&stalled.stderr).contains("deadline"));
    assert!(stalled.stdout.is_empty());
    drop(connection);
}
