use std::{
    os::unix::net::UnixListener,
    process::{Command, Output, Stdio},
    time::Instant,
};

use super::*;

const TUNING: [&str; 4] = [
    "DEKOPON_WORKER_THREADS",
    "DEKOPON_BLOCKING_THREADS",
    "DEKOPON_BLOCKING_KEEP_ALIVE",
    "DEKOPON_SHELL_THREADS",
];

fn env_file(directory: &Path, name: &str, contents: &str) -> PathBuf {
    let path = directory.join(name);
    write_owner_only(&path, contents.as_bytes());
    path
}

fn fifo(directory: &Path) -> PathBuf {
    let path = directory.join("blocking.env");
    assert!(
        Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo runs")
            .success()
    );
    path
}

fn gateway() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dekopon-gatewayd"));
    for variable in TUNING {
        command.env_remove(variable);
    }
    command
        .env_remove("TOKIO_WORKER_THREADS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn finished(command: &mut Command) -> Output {
    let mut child = command.spawn().expect("gateway starts");
    let until = Instant::now() + Duration::from_secs(20);
    while child.try_wait().expect("gateway status").is_none() {
        if Instant::now() >= until {
            child.kill().expect("stop a stalled gateway");
            child.wait().expect("reap a stalled gateway");
            panic!("the gateway did not exit; did it open a FIFO?");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().expect("gateway output")
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn every_precedence_combination_resolves_in_order() {
    const PAST_CLAP: i32 = 1;
    const REFUSED: i32 = 2;
    let directory = temporary();
    let config = directory.path().join("absent").join("gateway.json");
    for (row, cli, inherited, file, expected) in [
        ("file only", None, None, Some("0"), REFUSED),
        (
            "inherited beats file",
            None,
            Some("2"),
            Some("0"),
            PAST_CLAP,
        ),
        (
            "inherited invalid beats valid file",
            None,
            Some("0"),
            Some("2"),
            REFUSED,
        ),
        (
            "inherited empty beats file",
            None,
            Some(""),
            Some("2"),
            REFUSED,
        ),
        ("CLI beats file", Some("2"), None, Some("0"), PAST_CLAP),
        ("CLI beats inherited", Some("2"), Some("0"), None, PAST_CLAP),
        (
            "CLI beats inherited empty",
            Some("2"),
            Some(""),
            None,
            PAST_CLAP,
        ),
        ("CLI beats both", Some("2"), Some("0"), Some("0"), PAST_CLAP),
        ("none", None, None, None, PAST_CLAP),
    ] {
        let contents = file.map_or_else(
            || "UNRELATED=1\n".to_owned(),
            |value| format!("DEKOPON_WORKER_THREADS={value}\n"),
        );
        let path = env_file(directory.path(), "precedence.env", &contents);
        let mut command = gateway();
        command
            .arg("--config")
            .arg(&config)
            .arg("--env-file")
            .arg(&path);
        if let Some(value) = cli {
            command.args(["--worker-threads", value]);
        }
        if let Some(value) = inherited {
            command.env("DEKOPON_WORKER_THREADS", value);
        }
        let output = finished(&mut command);
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{row}: {}",
            output_text(&output)
        );
    }
}

#[test]
fn a_relative_env_file_resolves_against_the_invocation_directory() {
    let directory = temporary();
    let elsewhere = temporary();
    env_file(directory.path(), "daemon.env", "DEKOPON_WORKER_THREADS=0\n");
    let config = directory.path().join("absent.json");
    let loaded = finished(
        gateway()
            .current_dir(directory.path())
            .arg("--config")
            .arg(&config)
            .args(["--env-file", "daemon.env"]),
    );
    assert_eq!(loaded.status.code(), Some(2), "{}", output_text(&loaded));
    let missing = finished(
        gateway()
            .current_dir(elsewhere.path())
            .arg("--config")
            .arg(&config)
            .args(["--env-file", "daemon.env"]),
    );
    assert_eq!(missing.status.code(), Some(1), "{}", output_text(&missing));
    assert!(missing.stdout.is_empty(), "{}", output_text(&missing));
}

#[test]
fn a_refused_env_file_stops_before_serving() {
    let directory = temporary();
    let uid = dekopon_brokerd::current_uid();
    let broker = UnixListener::bind(directory.path().join("broker.sock")).expect("fake broker");
    broker.set_nonblocking(true).expect("fake broker polls");
    write_owner_only(
        &directory.path().join("dekopon.yaml"),
        catalog_text().as_bytes(),
    );
    let config = directory.path().join("gateway.json");
    write_owner_only(
        &config,
        &serde_json::to_vec(&gateway_config_with(
            directory.path(),
            uid,
            "http://127.0.0.1:1/v1",
            None,
            "off",
            false,
        ))
        .expect("config serializes"),
    );
    let path = env_file(directory.path(), "malformed.env", "KEY=two words\n");
    let output = finished(
        gateway()
            .arg("--config")
            .arg(&config)
            .arg("--env-file")
            .arg(&path),
    );
    assert_eq!(output.status.code(), Some(1), "{}", output_text(&output));
    assert_eq!(
        broker.accept().map(|_| ()).map_err(|error| error.kind()),
        Err(std::io::ErrorKind::WouldBlock)
    );
    assert!(!directory.path().join("dev.sock").exists());
}

#[test]
fn helper_modes_refuse_the_env_file_before_opening_it() {
    let directory = temporary();
    let blocking = fifo(directory.path());
    let helpers: [&[&str]; 6] = [
        &["auth", "chatgpt", "login"],
        &["auth", "chatgpt", "status"],
        &["auth", "chatgpt", "logout"],
        &["auth", "chatgpt", "export", "--expose-credential"],
        &["check", "gateway.json"],
        &["check", "--probe", "gateway.json"],
    ];
    for helper in helpers {
        let output = finished(
            gateway()
                .current_dir(directory.path())
                .arg("--env-file")
                .arg(&blocking)
                .args(helper),
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{helper:?}: {}",
            output_text(&output)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--env-file"),
            "{helper:?}: {}",
            output_text(&output)
        );
    }
}

#[test]
fn help_and_version_open_no_env_file() {
    let directory = temporary();
    let blocking = fifo(directory.path());
    for flag in ["--help", "--version"] {
        let output = finished(gateway().arg("--env-file").arg(&blocking).arg(flag));
        assert_eq!(
            output.status.code(),
            Some(0),
            "{flag}: {}",
            output_text(&output)
        );
        assert!(!output.stdout.is_empty(), "{flag}");
    }
}

#[test]
fn reserved_keys_are_refused() {
    let directory = temporary();
    let config = directory.path().join("absent.json");
    for line in [
        "DEKOPON_SHELL_JQ_WORKER=1",
        "LD_PRELOAD=/nonexistent/preload.so",
        "HOME=/nonexistent",
    ] {
        let path = env_file(directory.path(), "reserved.env", &format!("{line}\n"));
        let output = finished(
            gateway()
                .arg("--config")
                .arg(&config)
                .arg("--env-file")
                .arg(&path),
        );
        assert_eq!(
            output.status.code(),
            Some(1),
            "{line}: {}",
            output_text(&output)
        );
        assert!(output.stdout.is_empty(), "{line}: {}", output_text(&output));
    }
}

#[test]
fn an_invalid_file_value_is_not_echoed() {
    const SECRET: &str = "sk-synthetic-7c1e";
    let directory = temporary();
    let config = directory.path().join("absent.json");
    for (contents, status) in [
        (format!("DEKOPON_BLOCKING_KEEP_ALIVE={SECRET}\n"), 2),
        (format!("TOKEN={SECRET} trailing\n"), 1),
    ] {
        let path = env_file(directory.path(), "invalid.env", &contents);
        let output = finished(
            gateway()
                .arg("--config")
                .arg(&config)
                .arg("--env-file")
                .arg(&path),
        );
        let text = output_text(&output);
        assert_eq!(output.status.code(), Some(status), "{text}");
        assert!(!text.contains(SECRET), "{text}");
    }
}

#[test]
fn a_file_supplied_ca_path_is_not_echoed() {
    const SENTINEL: &str = "sentinel-ca-4d1f";
    let directory = temporary();
    let uid = dekopon_brokerd::current_uid();
    write_owner_only(
        &directory.path().join("dekopon.yaml"),
        catalog_text().as_bytes(),
    );
    let mut document = gateway_config_with(
        directory.path(),
        uid,
        "http://127.0.0.1:1/v1",
        None,
        "off",
        false,
    );
    document["telemetry"] = json!({
        "endpoint": "http://127.0.0.1:1",
        "serviceName": "dekopon-gatewayd-test",
        "exportTimeoutMs": 1000,
    });
    let config = directory.path().join("gateway.json");
    write_owner_only(
        &config,
        &serde_json::to_vec(&document).expect("config serializes"),
    );
    let holder = directory.path().join(SENTINEL);
    fs::create_dir(&holder).expect("sentinel directory");
    let invalid = holder.join("invalid.pem");
    fs::write(&invalid, b"not a certificate").expect("invalid PEM");
    for certificate in [holder.join("missing.pem"), invalid] {
        let path = env_file(
            directory.path(),
            "telemetry.env",
            &format!("OTEL_EXPORTER_OTLP_CERTIFICATE={}\n", certificate.display()),
        );
        let output = finished(
            gateway()
                .arg("--config")
                .arg(&config)
                .arg("--env-file")
                .arg(&path),
        );
        let text = output_text(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert!(text.contains("OTEL_EXPORTER_OTLP_CERTIFICATE"), "{text}");
        assert!(!text.contains(SENTINEL), "{text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_key_follows_inherited_then_file() {
    const VARIABLE: &str = "DEKOPON_TEST_MODEL_KEY";
    let fixture = boot(vec![
        final_answer("Inherited key answered."),
        final_answer("File key answered."),
    ])
    .await;
    let directory = fixture.directory.path();
    let mut config: Value = serde_json::from_slice(
        &fs::read(directory.join("dekopon-gatewayd.json")).expect("gateway config"),
    )
    .expect("config JSON");
    config["models"][0]["apiKeyEnv"] = json!(VARIABLE);
    let path = env_file(
        directory,
        "gateway.env",
        &format!("{VARIABLE}=file-key-synthetic\n"),
    );
    for (socket, inherited, expected) in [
        (
            "inherited.sock",
            Some("inherited-key-synthetic"),
            "Inherited key answered.",
        ),
        ("file.sock", None, "File key answered."),
    ] {
        let socket = directory.join(socket);
        config["transports"][0]["socketPath"] = json!(socket);
        let config_path = directory.join("model-key.json");
        write_owner_only(
            &config_path,
            &serde_json::to_vec(&config).expect("config serializes"),
        );
        let mut command = gateway();
        command
            .arg("--config")
            .arg(&config_path)
            .arg("--env-file")
            .arg(&path)
            .env_remove(VARIABLE)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(value) = inherited {
            command.env(VARIABLE, value);
        }
        let mut child = GatewayChild(command.spawn().expect("gateway subprocess"));
        tokio::time::timeout(Duration::from_secs(10), async {
            while !socket.exists() {
                assert!(child.0.try_wait().expect("child status").is_none());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the loaded gateway serves");
        let reply = tokio::task::spawn_blocking(move || ask(&socket, MAPPED_SUBJECT, "which key?"))
            .await
            .expect("ask");
        assert_eq!(reply, expected);
        drop(child);
    }
    assert_eq!(
        *fixture.model_authorizations.lock(),
        [
            Some("Bearer inherited-key-synthetic".to_owned()),
            Some("Bearer file-key-synthetic".to_owned()),
        ]
    );
    fixture.shutdown().await;
}
