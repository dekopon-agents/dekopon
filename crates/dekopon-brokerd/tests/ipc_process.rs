//! Real broker/client subprocesses. On Linux root runs the distinct-UID acceptance;
//! ordinary unprivileged package tests exercise the owner-client subprocess path.
//! DEKOPON_REQUIRE_CROSS_UID=1 makes missing UID-switch authority a hard failure.
use std::{
    fs,
    os::unix::{
        fs::{MetadataExt as _, PermissionsExt as _, chown},
        process::CommandExt as _,
    },
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use dekopon_broker_protocol::{BrokerClient, ClientError, ERROR_UNAUTHENTICATED, FrameLimits};
use serde_json::json;

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().expect("inspect child exit").is_none() {
            self.0.kill().expect("stop fixture child");
        }
        self.0.wait().expect("reap fixture child");
    }
}

fn wait(child: &mut Child) {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "IPC client failed: {status}");
            return;
        }
        assert!(Instant::now() < until, "IPC client exceeded deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn owned(path: &Path, uid: u32, gid: u32, mode: u32, root: bool) {
    if root {
        chown(path, Some(uid), Some(gid)).unwrap();
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[tokio::test]
async fn ipc_process_boundary() {
    if let Ok(role) = std::env::var("DEKOPON_IPC_TEST_ROLE") {
        let socket = std::env::var("DEKOPON_IPC_TEST_SOCKET").unwrap();
        let uid: u32 = std::env::var("DEKOPON_IPC_TEST_SERVER_UID")
            .unwrap()
            .parse()
            .unwrap();
        if role == "unmapped" {
            let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
            let response = dekopon_broker_protocol::read_frame::<
                _,
                dekopon_broker_protocol::ResponseEnvelope,
            >(&mut stream, FrameLimits::default())
            .await
            .unwrap();
            assert!(
                matches!(response.response, dekopon_broker_protocol::BrokerResponse::Error { code, .. } if code == ERROR_UNAUTHENTICATED)
            );
            return;
        }
        let client = BrokerClient::new(
            socket,
            uid,
            FrameLimits {
                max_frame_bytes: 2 * 1024 * 1024,
                io_timeout: Duration::from_secs(2),
            },
        )
        .unwrap();
        let result = client.capabilities().await;
        match role.as_str() {
            "mapped" | "owner" => assert_eq!(result.expect("mapped OS peer accepted").len(), 1),
            "wrong-pin" | "wrong-group" => {
                assert!(matches!(result, Err(ClientError::UnsafeSocket)))
            }
            "wrong-server" => assert!(
                matches!(result, Err(ClientError::ServerIdentity { expected, actual }) if expected == uid && actual == 0)
            ),
            _ => panic!("unknown fixture role"),
        }
        if role == "mapped" && std::env::var_os("DEKOPON_IPC_TEST_PRIVATE").is_some() {
            let private =
                std::path::PathBuf::from(std::env::var("DEKOPON_IPC_TEST_PRIVATE").unwrap());
            for name in ["credentials.json", "broker.json", "storage/sentinel"] {
                assert_eq!(
                    fs::read(private.join(name)).unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "gateway cannot read {name}"
                );
                assert_eq!(
                    fs::OpenOptions::new()
                        .write(true)
                        .open(private.join(name))
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "gateway cannot write {name}"
                );
            }
            assert_eq!(
                fs::write(private.join("storage/forged"), b"no")
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
            let parent = Path::new(&std::env::var("DEKOPON_IPC_TEST_SOCKET").unwrap())
                .parent()
                .unwrap()
                .to_path_buf();
            assert_eq!(
                fs::write(parent.join("forged"), b"no").unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied,
                "IPC group cannot replace listener"
            );
        }
        return;
    }
    let root = dekopon_brokerd::current_uid() == 0;
    if std::env::var_os("DEKOPON_REQUIRE_CROSS_UID").is_some() {
        assert!(
            root && cfg!(target_os = "linux"),
            "distinct-UID acceptance requires Linux root in a disposable container"
        );
    }
    let server_uid = if root {
        65532
    } else {
        dekopon_brokerd::current_uid()
    };
    let client_uid = if root { 65533 } else { server_uid };
    let gid = if root {
        65534
    } else {
        rustix::process::getegid().as_raw()
    };
    let directory = tempfile::tempdir().unwrap();
    owned(directory.path(), server_uid, gid, 0o755, root);
    let private = directory.path().join("private");
    let ipc = directory.path().join("ipc");
    fs::create_dir(&private).unwrap();
    fs::create_dir(&ipc).unwrap();
    owned(&private, server_uid, gid, 0o700, root);
    owned(&ipc, server_uid, gid, 0o710, root);
    let storage = private.join("storage");
    fs::create_dir(&storage).unwrap();
    owned(&storage, server_uid, gid, 0o700, root);
    fs::write(storage.join("sentinel"), "private storage").unwrap();
    owned(&storage.join("sentinel"), server_uid, gid, 0o600, root);
    let provider = private.join("echo.wasm");
    fs::copy(
        dekopon_test_support::provider_fixture("echo-provider.wasm"),
        &provider,
    )
    .unwrap();
    let credentials = private.join("credentials.json");
    fs::write(&credentials, serde_json::to_vec(&json!({
        "apiVersion": "dekopon.dev/broker-credentials/v1alpha1",
        "credentials": [{"name": "fixture-token", "kind": "bearerToken", "scheme": "Bearer", "destinations": ["example.com"], "secret": "IPC-PRIVATE-SENTINEL"}]
    })).unwrap()).unwrap();
    let policy = private.join("policy.cedar");
    fs::write(&policy, r#"permit(principal == Dekopon::Principal::"caller", action == Dekopon::Action::"echo.echo", resource == Dekopon::Provider::"echo");"#).unwrap();
    let socket = ipc.join("broker.sock");
    let config = private.join("broker.json");
    let mut identities = vec![
        json!({"uid": client_uid, "principal": "caller", "actor": {"type": "service", "principal": "caller"}}),
    ];
    if root {
        identities.push(json!({"uid": server_uid, "principal": "caller", "actor": {"type": "service", "principal": "caller"}}));
    }
    fs::write(&config, serde_json::to_vec(&json!({
        "apiVersion": "dekopon.dev/brokerd/v1alpha1", "socketPath": socket,
        "brokerPrincipal": "broker", "policyRevision": "ipc-test", "policiesPath": policy,
        "providers": [provider], "credentialsPath": credentials, "identities": identities,
        "constraintSets": {"echo.echo": {"provider": "echo", "effect": "read-only", "risk": "Low", "constraints": {"timeoutMs": 30000, "maxOutputBytes": 1048576}}}
    })).unwrap()).unwrap();
    for path in [&provider, &credentials, &policy, &config] {
        owned(path, server_uid, gid, 0o600, root);
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_dekopon-brokerd"));
    command
        .args(["--config", config.to_str().unwrap()])
        .env("RUST_LOG", "error")
        .stdin(Stdio::null());
    if root {
        command.uid(server_uid).gid(gid);
    }
    let mut broker = Process(command.spawn().unwrap());
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            broker.0.try_wait().unwrap().is_none(),
            "broker exited before ready"
        );
        if fs::symlink_metadata(&socket).is_ok_and(|m| m.permissions().mode() & 0o7777 == 0o660) {
            break;
        }
        assert!(Instant::now() < until, "broker socket readiness deadline");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let metadata = fs::symlink_metadata(&socket).unwrap();
    assert_eq!(metadata.uid(), server_uid);
    assert_eq!(metadata.gid(), gid);
    let run_client = |role: &str, uid: u32, pin: u32, socket: &Path| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "ipc_process_boundary", "--nocapture"])
            .env("DEKOPON_IPC_TEST_ROLE", role)
            .env("DEKOPON_IPC_TEST_SOCKET", socket)
            .env("DEKOPON_IPC_TEST_SERVER_UID", pin.to_string())
            .stdin(Stdio::null());
        if root {
            command
                .uid(uid)
                .gid(gid)
                .env("DEKOPON_IPC_TEST_PRIVATE", &private);
        }
        let mut client = Process(command.spawn().unwrap());
        wait(&mut client.0);
    };
    let mut probe = Command::new(env!("CARGO_BIN_EXE_dekopon-brokerd"));
    probe.args(["probe", "--socket", socket.to_str().unwrap()]);
    if root {
        probe.uid(server_uid).gid(gid);
    }
    let mut probe = Process(probe.spawn().unwrap());
    wait(&mut probe.0);
    run_client("mapped", client_uid, server_uid, &socket);
    run_client("owner", server_uid, server_uid, &socket);
    run_client("wrong-pin", client_uid, server_uid.wrapping_add(1), &socket);
    if root {
        assert_ne!(client_uid, server_uid);
        run_client("unmapped", 65530, server_uid, &socket);
        // Root can forge filesystem ownership, but cannot forge the live peer credentials.
        let fake = ipc.join("counterfeit.sock");
        let listener = tokio::net::UnixListener::bind(&fake).unwrap();
        owned(&fake, server_uid, gid, 0o660, root);
        run_client("wrong-server", client_uid, server_uid, &fake);
        let (mut connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        use tokio::io::AsyncReadExt as _;
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), connection.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0,
            "wrong server receives no request bytes"
        );
        chown(&fake, None, Some(65529)).unwrap();
        run_client("wrong-group", client_uid, server_uid, &fake);
        println!(
            "DISTINCT-UID PASS: broker=65532 gateway=65533 IPC-group=65534; mapped, owner, unmapped, pin, live-peer, gid and private-file boundaries"
        );
    } else {
        println!(
            "OWNER-UID ONLY: distinct-UID acceptance was not exercised; run with DEKOPON_REQUIRE_CROSS_UID=1 in Linux root container"
        );
    }
}
