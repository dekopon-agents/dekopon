#![allow(clippy::unwrap_used)]

use std::{
    ffi::OsStr,
    fs,
    os::unix::{fs::PermissionsExt as _, process::CommandExt as _},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use dekopon_test_support::provider_fixture;
use serde_json::{Value, json};

const TUNING: [&str; 4] = [
    "DEKOPON_WORKER_THREADS",
    "DEKOPON_BLOCKING_THREADS",
    "DEKOPON_BLOCKING_KEEP_ALIVE",
    "DEKOPON_CPU_THREADS",
];
const ARGV0: &str = "dekopon-brokerd-under-test";

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

fn write_owner_only(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn private_directory() -> tempfile::TempDir {
    let parent = std::env::temp_dir().canonicalize().unwrap();
    let directory = tempfile::Builder::new().tempdir_in(parent).unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

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
            .unwrap()
            .success()
    );
    path
}

fn config_document(private: &Path) -> Value {
    let provider = private.join("cli-probe-provider.wasm");
    fs::copy(provider_fixture("cli-probe-provider.wasm"), &provider).unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o600)).unwrap();
    let policies = private.join("policies.cedar");
    write_owner_only(
        &policies,
        br#"@id("cpetersen-upper")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"cli-probe.upper",
       resource == Dekopon::Provider::"cli-probe");"#,
    );
    json!({
        "apiVersion": dekopon_brokerd::CONFIG_API_VERSION,
        "socketPath": private.join("broker.sock"),
        "policiesPath": policies,
        "providers": [provider],
        "identities": [{
            "uid": dekopon_brokerd::current_uid(),
            "principal": "caller",
            "actor": {"type": "agent", "agent": "brokerd-test"},
            "attestor": {},
        }],
        "principals": {"cpetersen": {"subjects": ["slack.t0123abc.u9xyz"]}},
        "capabilities": {
            "cli-probe": {
                "constraints": {"timeoutMs": 30000},
                "capabilities": {"cli-probe.upper": {}},
            }
        },
    })
}

fn write_config(private: &Path, document: &Value) -> PathBuf {
    let config = private.join("broker.json");
    write_owner_only(&config, &serde_json::to_vec(document).unwrap());
    config
}

fn broker() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dekopon-brokerd"));
    for variable in TUNING {
        command.env_remove(variable);
    }
    command
        .env_remove("TOKIO_WORKER_THREADS")
        .env_remove("RAYON_NUM_THREADS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn finished(command: &mut Command) -> Output {
    let mut child = command.spawn().unwrap();
    let until = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= until {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("the broker did not exit; did it open a FIFO?");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn serving(command: &mut Command, socket: &Path) -> Process {
    let mut process = Process(
        command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + Duration::from_secs(30);
    while fs::symlink_metadata(socket).is_err() {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "the broker exited before binding"
        );
        assert!(Instant::now() < until, "the broker did not bind its socket");
        thread::sleep(Duration::from_millis(10));
    }
    process
}

fn terminated(mut process: Process) -> ExitStatus {
    let pid = rustix::process::Pid::from_child(&process.0);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < until, "the broker ignored SIGTERM");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_refused_env_file_stops_before_serving() {
    let directory = private_directory();
    let config = write_config(directory.path(), &config_document(directory.path()));
    let path = env_file(directory.path(), "malformed.env", "KEY=two words\n");
    let output = finished(
        broker()
            .arg("--config")
            .arg(&config)
            .arg("--env-file")
            .arg(&path),
    );
    assert_eq!(output.status.code(), Some(1), "{}", output_text(&output));
    assert!(fs::symlink_metadata(directory.path().join("broker.sock")).is_err());
}

#[test]
fn the_loaded_daemon_keeps_its_pid_drops_the_option_and_serves() {
    let directory = private_directory();
    let config = write_config(directory.path(), &config_document(directory.path()));
    let socket = directory.path().join("broker.sock");
    let path = env_file(
        directory.path(),
        "broker.env",
        "DEKOPON_BLOCKING_THREADS=16\n",
    );
    let process = serving(
        broker()
            .arg0(ARGV0)
            .arg("--config")
            .arg(&config)
            .arg("--env-file")
            .arg(&path),
        &socket,
    );
    let pid = process.0.id();
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    let arguments: Vec<&OsStr> = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(<OsStr as std::os::unix::ffi::OsStrExt>::from_bytes)
        .collect();
    assert_eq!(
        arguments,
        [
            OsStr::new(ARGV0),
            OsStr::new("--config"),
            config.as_os_str()
        ]
    );
    let environ = fs::read(format!("/proc/{pid}/environ")).unwrap();
    assert!(
        environ
            .split(|byte| *byte == 0)
            .any(|entry| entry == b"DEKOPON_BLOCKING_THREADS=16")
    );
    let probe = finished(broker().arg("probe").arg("--socket").arg(&socket));
    assert!(probe.status.success(), "{}", output_text(&probe));
    assert_eq!(process.0.id(), pid);
}

#[test]
fn sigterm_after_reexec_shuts_down_like_a_direct_start() {
    let directory = private_directory();
    let config = write_config(directory.path(), &config_document(directory.path()));
    let socket = directory.path().join("broker.sock");
    let path = env_file(
        directory.path(),
        "broker.env",
        "DEKOPON_BLOCKING_THREADS=16\n",
    );
    let direct = terminated(serving(broker().arg("--config").arg(&config), &socket));
    assert!(fs::symlink_metadata(&socket).is_err());
    let loaded = terminated(serving(
        broker()
            .arg("--config")
            .arg(&config)
            .arg("--env-file")
            .arg(&path),
        &socket,
    ));
    assert!(fs::symlink_metadata(&socket).is_err());
    assert_eq!(loaded.code(), direct.code());
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&loaded),
        std::os::unix::process::ExitStatusExt::signal(&direct)
    );
}

#[test]
fn helper_modes_refuse_the_env_file_before_opening_it() {
    let directory = private_directory();
    let blocking = fifo(directory.path());
    let store = ["--lock-file", "providers.lock", "--store", "store"];
    let helpers: [Vec<&str>; 7] = [
        vec!["probe", "--socket", "broker.sock"],
        [
            &["provider", "sync", "--provider-set", "set.yaml"][..],
            &store,
        ]
        .concat(),
        [&["provider", "list"][..], &store].concat(),
        [&["provider", "precompile"][..], &store].concat(),
        [&["provider", "skills", "--out", "skills"][..], &store].concat(),
        [&["provider", "verify"][..], &store].concat(),
        vec!["check", "broker.json"],
    ];
    for helper in helpers {
        let output = finished(
            broker()
                .current_dir(directory.path())
                .arg("--env-file")
                .arg(&blocking)
                .args(&helper),
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
    let directory = private_directory();
    let blocking = fifo(directory.path());
    for flag in ["--help", "--version"] {
        let output = finished(broker().arg("--env-file").arg(&blocking).arg(flag));
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
fn an_invalid_file_value_is_not_echoed() {
    const SECRET: &str = "sk-synthetic-7c1e";
    let directory = private_directory();
    let config = directory.path().join("absent.json");
    for (contents, status) in [
        (format!("DEKOPON_BLOCKING_KEEP_ALIVE={SECRET}\n"), 2),
        (format!("TOKEN={SECRET} trailing\n"), 1),
    ] {
        let path = env_file(directory.path(), "invalid.env", &contents);
        let output = finished(
            broker()
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
    let directory = private_directory();
    let mut document = config_document(directory.path());
    document["telemetry"] = json!({
        "endpoint": "http://127.0.0.1:1",
        "serviceName": "dekopon-brokerd-test",
        "exportTimeoutMs": 1000,
    });
    let config = write_config(directory.path(), &document);
    let holder = directory.path().join(SENTINEL);
    fs::create_dir(&holder).unwrap();
    let invalid = holder.join("invalid.pem");
    fs::write(&invalid, b"not a certificate").unwrap();
    for certificate in [holder.join("missing.pem"), invalid] {
        let path = env_file(
            directory.path(),
            "telemetry.env",
            &format!("OTEL_EXPORTER_OTLP_CERTIFICATE={}\n", certificate.display()),
        );
        let output = finished(
            broker()
                .arg("--config")
                .arg(&config)
                .arg("--env-file")
                .arg(&path),
        );
        let text = output_text(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert!(text.contains("OTEL_EXPORTER_OTLP_CERTIFICATE"), "{text}");
        assert!(!text.contains(SENTINEL), "{text}");
        assert!(fs::symlink_metadata(directory.path().join("broker.sock")).is_err());
    }
}

#[test]
fn helper_modes_refuse_the_log_file_and_create_nothing() {
    let directory = private_directory();
    let log = directory.path().join("broker.log");
    let helpers: [&[&str]; 5] = [
        &["probe", "--socket", "broker.sock"],
        &["check", "broker.json"],
        &[
            "provider",
            "--lock-file",
            "lock",
            "--store",
            "store",
            "verify",
        ],
        &["--help"],
        &["--version"],
    ];
    for helper in helpers {
        let output = finished(
            broker()
                .current_dir(directory.path())
                .arg("--log-file")
                .arg(&log)
                .args(helper),
        );
        let expected = if helper[0].starts_with("--") { 0 } else { 2 };
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{helper:?}: {}",
            output_text(&output)
        );
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn an_unsafe_log_directory_stops_the_broker_before_serving() {
    let directory = private_directory();
    let config = write_config(directory.path(), &config_document(directory.path()));
    let shared = directory.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    let output = finished(
        broker()
            .arg("--config")
            .arg(&config)
            .arg("--log-file")
            .arg(shared.join("broker.log")),
    );
    assert_eq!(output.status.code(), Some(1), "{}", output_text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains(&*shared.to_string_lossy()));
    assert!(fs::symlink_metadata(directory.path().join("broker.sock")).is_err());
    assert_eq!(fs::read_dir(&shared).unwrap().count(), 0);
}

#[test]
fn a_serving_broker_writes_console_json_to_its_log_file() {
    let directory = private_directory();
    let config = write_config(directory.path(), &config_document(directory.path()));
    let socket = directory.path().join("broker.sock");
    let log = directory.path().join("broker.log");
    let process = serving(
        broker()
            .arg("--config")
            .arg(&config)
            .arg("--log-file")
            .arg(&log),
        &socket,
    );
    assert!(terminated(process).success());
    let metadata = fs::metadata(&log).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let records: Vec<Value> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!records.is_empty());
    assert!(records.iter().all(|record| record["level"].is_string()));
}
