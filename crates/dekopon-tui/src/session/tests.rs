use std::path::PathBuf;

use dekopon_broker_protocol::FrameLimits;
use dekopon_model::chatgpt;
use dekopon_shell::CapabilityInvoker as _;
use serde_json::json;

use super::{
    CONSOLE_AUTH_FILE_NAME, NoDirect, SessionError, StopFlag, guard_shared_credential,
    resolve_console_credential,
};

#[test]
fn an_explicit_path_is_honoured_wherever_it_points() {
    // Including at the shared file: an explicit path is the deliberate, typed act this CLI already
    // uses for every other credential decision.
    let shared = PathBuf::from("/config/dekopon/chatgpt-auth.json");
    assert_eq!(
        resolve_console_credential(Some(&shared)).expect("explicit paths are honoured"),
        shared
    );
}

#[test]
fn console_and_shared_resolution_differ_only_in_the_leaf() {
    // Pinned against the model crate's own answer rather than a copied literal, so the two cannot
    // drift into resolving to different directories.
    let console = chatgpt::resolve_auth_path_named(None, CONSOLE_AUTH_FILE_NAME);
    let shared = chatgpt::resolve_auth_path_named(None, chatgpt::DEFAULT_AUTH_FILE_NAME);
    let (Ok(console), Ok(shared)) = (console, shared) else {
        // No HOME, XDG_CONFIG_HOME, or APPDATA in this environment; there is nothing to compare.
        return;
    };
    if std::env::var_os("DEKOPON_CHATGPT_AUTH_FILE").is_some() {
        assert_eq!(console, shared, "the environment tier is verbatim for both");
        return;
    }
    assert_eq!(console.parent(), shared.parent());
    assert_eq!(
        console.file_name().and_then(std::ffi::OsStr::to_str),
        Some(CONSOLE_AUTH_FILE_NAME)
    );
    assert_ne!(console, shared);
}

#[test]
fn discovery_landing_on_the_shared_file_is_refused_by_name() {
    // The environment tier returns its value verbatim for any file name, so an exported
    // DEKOPON_CHATGPT_AUTH_FILE is the one way discovery reaches the gateway's credential.
    let shared = PathBuf::from("/config/dekopon/chatgpt-auth.json");
    let refused = guard_shared_credential(shared.clone(), &shared, false);

    let Err(SessionError::SharedCredential { path }) = refused else {
        panic!("discovery onto the shared credential must be refused: {refused:?}");
    };
    assert_eq!(path, shared);

    let message = SessionError::SharedCredential {
        path: shared.clone(),
    }
    .to_string();
    assert!(
        message.contains(&shared.display().to_string()),
        "the refusal must name the path an operator will go and look at: {message}"
    );
    assert!(
        message.contains("--auth-file"),
        "the refusal must name the way out it accepts: {message}"
    );
    assert!(
        message.contains("rotates"),
        "the refusal must say why, or it reads as arbitrary: {message}"
    );
}

#[test]
fn an_explicit_path_defeats_the_guard_deliberately() {
    let shared = PathBuf::from("/config/dekopon/chatgpt-auth.json");
    assert_eq!(
        guard_shared_credential(shared.clone(), &shared, true).expect("explicit is deliberate"),
        shared
    );
}

#[test]
fn a_distinct_console_path_passes_the_guard() {
    let console = PathBuf::from("/config/dekopon/chatgpt-auth.console.json");
    let shared = PathBuf::from("/config/dekopon/chatgpt-auth.json");
    assert_eq!(
        guard_shared_credential(console.clone(), &shared, false).expect("distinct files are fine"),
        console
    );
}

#[test]
fn the_empty_local_leg_claims_nothing() {
    // Every claim it could make would be a claim the broker never authorized, so it makes none and
    // dispatch falls through to the leg that has authority.
    let direct = NoDirect;
    assert!(direct.granted().is_empty());
    assert!(direct.command_words().is_empty());
    assert!(!direct.is_granted("gh.issue.list"));
    assert!(!direct.grants_namespace("gh"));
    assert!(!direct.has_command_word("gh"));
    assert!(direct.describe("gh.issue.list").is_none());
    assert!(matches!(
        direct.invoke("gh.issue.list", json!({})),
        dekopon_shell::CapabilityCallResult::NotFound
    ));
}

#[test]
fn the_stop_flag_latches_and_resets() {
    use dekopon_agent::prompt::CancellationProbe as _;

    let stop = StopFlag::default();
    assert!(!stop.is_cancelled());
    stop.request();
    assert!(
        stop.is_cancelled(),
        "a requested stop must be visible to the loop"
    );
    assert!(stop.clone().is_cancelled(), "clones share one flag");
    stop.reset();
    assert!(!stop.is_cancelled(), "the next session starts uncancelled");
}

#[test]
fn an_unresolved_socket_names_both_ways_out() {
    assert!(
        SessionError::SocketUnresolved
            .to_string()
            .contains("pass --socket or set DEKOPON_BROKER_SOCKET")
    );
}

#[tokio::test]
async fn a_stopped_broker_is_refused_by_the_exact_path_that_was_tried() {
    // The whole point of probing at startup: without it this failure would arrive on the first hop,
    // after the console had taken the screen, as a refusal naming nothing an operator can act on.
    let directory = tempfile::tempdir().expect("temporary directory");
    let absent = directory.path().join("broker.sock");
    let mut options = super::ConsoleOptions::new(
        "tel.15550100000".parse().expect("valid subject fixture"),
        "test-model".to_owned(),
    );
    options.socket = Some(absent.clone());

    let refused = super::connect(&options).await;
    let Err(error @ SessionError::NoBroker { .. }) = refused else {
        panic!("an absent socket must be refused: {refused:?}");
    };
    let SessionError::NoBroker { path, tier, .. } = &error else {
        unreachable!("matched above")
    };
    assert_eq!(
        path, &absent,
        "the refusal must name the path it tried, not a candidate it might have tried"
    );
    assert_eq!(*tier, "explicit");

    let message = error.to_string();
    assert!(message.contains("no broker found at"), "got: {message}");
    assert!(
        message.contains(&absent.display().to_string()),
        "got: {message}"
    );
    assert!(message.contains("explicit"), "got: {message}");

    // And the reason the probe has to exist at all: constructing a client against that same absent
    // path succeeds, because `BrokerClient::new` validates a path's ownership and mode rather than
    // connecting. Without one exchange, "no broker" would surface on the first hop instead.
    assert!(
        dekopon_broker_protocol::BrokerClient::new(&absent, 0, FrameLimits::default()).is_ok(),
        "if constructing a client ever starts probing, this test's premise is stale"
    );
}
