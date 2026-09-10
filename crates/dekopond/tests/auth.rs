#![cfg(unix)]

use std::{
    path::PathBuf,
    process::{Command, Output},
};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dekopond"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

#[test]
fn chatgpt_auth_status_does_not_require_configuration() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = directory.path().join("missing-auth.json");
    let output = binary()
        .current_dir(directory.path())
        .args(["auth", "chatgpt", "status", "--auth-file"])
        .arg(&auth_file)
        .args(["--output", "json"])
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let status: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("auth status JSON parses");
    assert_eq!(status["account"], "chatgpt");
    assert_eq!(status["signedIn"], false);
    assert_eq!(status["credentialFile"], auth_file.display().to_string());
}

/// One deterministic credential file, written in the field order the export re-serializes, so the
/// exported document is byte-identical to the fixture.
const CREDENTIAL_FIXTURE: &str = concat!(
    r#"{"version":1,"access":"access-token-fixture","refresh":"refresh-token-fixture","#,
    r#""expiresAt":1700000000,"accountId":"acct-fixture"}"#,
    "\n"
);

/// The same document, base64-encoded, as the emitted Secret must carry it.
const CREDENTIAL_FIXTURE_BASE64: &str = concat!(
    "eyJ2ZXJzaW9uIjoxLCJhY2Nlc3MiOiJhY2Nlc3MtdG9rZW4tZml4dHVyZSIsInJlZnJlc2giOiJy",
    "ZWZyZXNoLXRva2VuLWZpeHR1cmUiLCJleHBpcmVzQXQiOjE3MDAwMDAwMDAsImFjY291bnRJZCI6",
    "ImFjY3QtZml4dHVyZSJ9Cg=="
);

fn credential_fixture(directory: &std::path::Path, contents: &str) -> PathBuf {
    let path = directory.join("chatgpt-auth.json");
    std::fs::write(&path, contents).expect("write credential fixture");
    path
}

fn export(auth_file: &std::path::Path, arguments: &[&str]) -> Output {
    binary()
        .args(["auth", "chatgpt", "export", "--no-color", "--auth-file"])
        .arg(auth_file)
        .args(arguments)
        .output()
        .expect("CLI process starts")
}

/// The Secret manifest is applied by `kubectl` and diffed by hand, so its bytes are the contract.
/// The comment header is part of it: a manifest saved to a file outlives the terminal that warned.
#[test]
fn chatgpt_export_emits_an_exact_secret_manifest() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(&auth_file, &["--expose-credential"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "# Exported by `dekopond auth chatgpt export`. This manifest carries a live ChatGPT access token and\n\
             # a rotating refresh token; base64 here is Kubernetes' encoding for `data`, not encryption.\n\
             #\n\
             # The refresh token rotates: whichever process refreshes next invalidates this copy. Seed it once\n\
             # into a writable directory, never overwrite a newer credential file with it, and re-export after\n\
             # a deliberate rotation.\n\
             apiVersion: v1\n\
             kind: Secret\n\
             metadata:\n  \
               name: dekopon-chatgpt-auth\n  \
               labels:\n    \
                 app.kubernetes.io/component: chatgpt-credential\n    \
                 app.kubernetes.io/managed-by: dekopon-auth-export\n    \
                 app.kubernetes.io/name: dekopon\n\
             type: Opaque\n\
             data:\n  \
               chatgpt-auth.json: {CREDENTIAL_FIXTURE_BASE64}\n"
        )
    );
}

/// `--namespace` is the only shape change the manifest accepts, and it must land in `metadata`
/// rather than anywhere a reader would miss it.
#[test]
fn chatgpt_export_places_the_secret_in_a_namespace() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(
        &auth_file,
        &[
            "--expose-credential",
            "--namespace",
            "dekopon",
            "--secret-name",
            "chatgpt-seed",
        ],
    );

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("  name: chatgpt-seed\n  namespace: dekopon\n"));
}

/// The raw form is pasted into a password-manager field and later projected back into a file, so
/// it must be exactly the document a login would have written — no wrapper, no re-indentation.
#[test]
fn chatgpt_export_emits_the_exact_credential_document() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(&auth_file, &["--expose-credential", "--format", "raw"]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), CREDENTIAL_FIXTURE);
}

/// Every export must say that the copy it just produced dies at the next refresh, and must say it
/// on standard error so the document stays pipeable.
#[test]
fn chatgpt_export_warns_that_the_exported_copy_rotates() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(&auth_file, &["--expose-credential", "--format", "raw"]);

    let diagnostics = stderr(&output);
    assert!(diagnostics.contains("rotates"), "{diagnostics}");
    assert!(diagnostics.contains("in the clear"), "{diagnostics}");
    assert!(!stdout(&output).contains("rotates"));
}

/// Printing a credential must be typed out, not defaulted into.
#[test]
fn chatgpt_export_requires_the_credential_acknowledgement() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(&auth_file, &[]);

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--expose-credential"));
    assert!(stdout(&output).is_empty());
}

/// No credential must fail loudly. An empty or half-formed Secret is the failure that survives
/// into a cluster and fails later, somewhere less obvious.
#[test]
fn chatgpt_export_without_a_credential_fails() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = directory.path().join("missing-auth.json");

    let output = export(&auth_file, &["--expose-credential"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("not logged in to ChatGPT"));
    assert!(stdout(&output).is_empty());
}

/// A credential file that is not credential JSON must name the file rather than emit a manifest.
#[test]
fn chatgpt_export_rejects_a_malformed_credential_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), "{ not json");

    let output = export(&auth_file, &["--expose-credential"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("could not parse ChatGPT credentials"));
    assert!(stdout(&output).is_empty());
}

/// Valid JSON with empty tokens is the more dangerous malformed case, because it would otherwise
/// produce a structurally perfect Secret carrying nothing.
#[test]
fn chatgpt_export_rejects_an_incomplete_credential_file() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(
        directory.path(),
        r#"{"version":1,"access":"","refresh":"","expiresAt":0,"accountId":""}"#,
    );

    let output = export(&auth_file, &["--expose-credential"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("incomplete"));
    assert!(stdout(&output).is_empty());
}

/// `--quiet` would suppress the document and still exit zero, so a scripted seeding step would
/// store nothing and believe it had succeeded.
#[test]
fn chatgpt_export_refuses_to_be_quiet() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    let output = export(&auth_file, &["--expose-credential", "--quiet"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("--quiet"));
}

/// A name the API server would reject must fail before the credential is read, not after it has
/// been printed and piped somewhere.
///
/// The dotted cases are the ones a whole-string character filter lets through: DNS-1123 applies
/// its start/end rule to every label, not just to the first and last character of the name.
#[test]
fn chatgpt_export_rejects_an_invalid_secret_name() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);

    for name in ["Not_A_Name", "a.-b.c", "a-.b", "a..b", "-a", "a-"] {
        let output = export(&auth_file, &["--expose-credential", "--secret-name", name]);

        assert_eq!(output.status.code(), Some(2), "{name}");
        assert!(stdout(&output).is_empty(), "{name}");
    }

    let output = export(
        &auth_file,
        &["--expose-credential", "--secret-name", "a-b.c9.d"],
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).contains("a-b.c9.d"));
}

/// The command's own help must say that it prints credential material.
#[test]
fn chatgpt_export_help_states_that_it_prints_a_credential() {
    let output = binary()
        .args(["auth", "chatgpt", "export", "--help"])
        .output()
        .expect("CLI process starts");

    assert_eq!(output.status.code(), Some(0));
    let help = stdout(&output);
    assert!(
        help.contains("prints real credential material in the clear"),
        "{help}"
    );
    assert!(help.contains("--expose-credential"), "{help}");
}
#[test]
fn auth_isolated_from_gateway_config_transport_and_telemetry_discovery() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = directory.path().join("missing-auth.json");
    let output = binary()
        .env_clear()
        .env(
            "DEKOPON_CONFIG",
            directory.path().join("absent-catalog.yaml"),
        )
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "not an endpoint")
        .current_dir(directory.path())
        .arg("--config")
        .arg(directory.path()) // A directory is not a usable gateway configuration.
        .args(["auth", "chatgpt", "status", "--auth-file"])
        .arg(&auth_file)
        .args(["-o", "json"])
        .output()
        .expect("gateway auth starts without any transport credential environment");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
    let status: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("only status JSON");
    assert_eq!(status["signedIn"], false);
    assert_eq!(status["expired"], false);
    assert!(!auth_file.exists());
}

#[test]
fn status_formats_and_logout_never_disclose_credentials() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let auth_file = credential_fixture(directory.path(), CREDENTIAL_FIXTURE);
    for format in ["table", "wide", "name", "json", "yaml"] {
        let output = binary()
            .env_clear()
            .args(["auth", "chatgpt", "status", "--auth-file"])
            .arg(&auth_file)
            .args(["-o", format])
            .output()
            .expect("status starts");
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        for token in [
            "access-token-fixture",
            "refresh-token-fixture",
            "acct-fixture",
        ] {
            assert!(!stdout(&output).contains(token));
            assert!(!stderr(&output).contains(token));
        }
        match format {
            "json" => {
                let status: serde_json::Value =
                    serde_json::from_slice(&output.stdout).expect("JSON");
                assert_eq!(status["signedIn"], true);
                assert_eq!(status["expired"], true);
            }
            "yaml" => {
                let status: serde_yaml::Value =
                    serde_yaml::from_slice(&output.stdout).expect("YAML");
                assert_eq!(status["signedIn"].as_bool(), Some(true));
                assert_eq!(status["expired"].as_bool(), Some(true));
            }
            "name" => assert_eq!(stdout(&output), "auth/chatgpt\n"),
            _ => assert!(stdout(&output).contains("signed in; refresh required")),
        }
    }
    let other = directory.path().join("other-client.json");
    std::fs::write(&other, "synthetic unrelated client").expect("other client fixture");
    for _ in 0..2 {
        let output = binary()
            .env_clear()
            .args(["auth", "chatgpt", "logout", "--auth-file"])
            .arg(&auth_file)
            .args(["-o", "json"])
            .output()
            .expect("logout starts");
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
        assert_eq!(status["signedIn"], false);
        assert_eq!(status["expired"], false);
        assert!(!auth_file.exists());
        assert_eq!(
            std::fs::read_to_string(&other).expect("other client remains"),
            "synthetic unrelated client"
        );
        assert!(!stdout(&output).contains("token-fixture"));
        assert!(!stderr(&output).contains("token-fixture"));
    }
}

#[test]
fn serving_requires_config_and_auth_flags_do_not_change_daemon_logging() {
    for arguments in [
        vec![],
        vec!["--quiet"],
        vec!["--output", "json"],
        vec!["get", "agents"],
    ] {
        let output = binary()
            .env_clear()
            .args(arguments)
            .output()
            .expect("usage starts");
        assert_eq!(output.status.code(), Some(2));
        assert!(stdout(&output).is_empty());
    }
    for operation in ["login", "status", "logout", "export"] {
        let output = binary()
            .env_clear()
            .args(["auth", "chatgpt", operation, "--help"])
            .output()
            .expect("help never starts device login");
        assert_eq!(output.status.code(), Some(0));
        assert!(stdout(&output).contains("--auth-file"));
        assert!(stderr(&output).is_empty());
    }
}

#[test]
fn malformed_credential_diagnostics_never_reflect_values() {
    use std::os::unix::fs::PermissionsExt;
    for field in ["version", "expiresAt"] {
        let directory = tempfile::tempdir().expect("temporary directory");
        let marker = format!("synthetic-malformed-{field}-only");
        let mut document: serde_json::Value =
            serde_json::from_str(CREDENTIAL_FIXTURE).expect("fixture JSON");
        document[field] = marker.clone().into();
        let bytes = serde_json::to_vec(&document).expect("encode synthetic document");
        let path = directory.path().join("malformed.json");
        std::fs::write(&path, &bytes).expect("write synthetic credential");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("private fixture");
        for command in ["status", "export"] {
            for verbosity in [None, Some("-v"), Some("-vv")] {
                let mut cli = binary();
                cli.env_clear()
                    .args(["auth", "chatgpt", command, "--no-color", "--auth-file"])
                    .arg(&path);
                if command == "export" {
                    cli.arg("--expose-credential");
                }
                if let Some(flag) = verbosity {
                    cli.arg(flag);
                }
                let output = cli.output().expect("CLI starts");
                let diagnostic = stderr(&output);
                assert_eq!(output.status.code(), Some(1));
                assert!(output.stdout.is_empty());
                for value in [
                    &marker,
                    "access-token-fixture",
                    "refresh-token-fixture",
                    "acct-fixture",
                ] {
                    assert!(!stdout(&output).contains(value));
                    assert!(!diagnostic.contains(value));
                }
                assert!(diagnostic.contains("could not parse ChatGPT credentials"));
                assert!(diagnostic.contains(path.to_str().expect("UTF-8 path")));
                if verbosity.is_some() {
                    assert!(diagnostic.contains("credential JSON Data at line 1 column "));
                }
                if verbosity == Some("-vv") {
                    assert!(diagnostic.contains("debug: ChatGpt::ParseAuth"));
                }
                assert_eq!(std::fs::read(&path).expect("read fixture"), bytes);
            }
        }
    }
}

#[test]
fn non_parse_credential_io_failure_keeps_cause_and_debug_context() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("loop.json");
    std::os::unix::fs::symlink(&path, &path).expect("self-referential symlink");
    let cause = std::fs::File::open(&path).expect_err("ELOOP").to_string();
    for command in ["status", "export"] {
        for verbosity in ["-v", "-vv"] {
            let mut cli = binary();
            cli.env_clear()
                .args([
                    "auth",
                    "chatgpt",
                    command,
                    verbosity,
                    "--no-color",
                    "--auth-file",
                ])
                .arg(&path);
            if command == "export" {
                cli.arg("--expose-credential");
            }
            let output = cli.output().expect("CLI starts");
            let diagnostic = stderr(&output);
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            assert!(diagnostic.contains("could not read ChatGPT credentials"));
            assert!(diagnostic.contains(path.to_str().expect("UTF-8 path")));
            assert!(diagnostic.contains(&format!("caused by: {cause}")));
            if verbosity == "-vv" {
                assert!(diagnostic.contains("debug:"));
                assert!(diagnostic.contains("ReadAuth"));
                assert!(diagnostic.contains("Os {"));
            }
            assert_eq!(std::fs::read_link(&path).expect("symlink survives"), path);
        }
    }
}
