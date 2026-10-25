use super::*;
use parking_lot::Mutex;
use std::collections::BTreeMap;

#[derive(Default)]
struct Commands(Mutex<Vec<Vec<Arg>>>);
impl CapabilityInvoker for Commands {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }
    fn command_words(&self) -> Vec<String> {
        vec!["asset".to_owned()]
    }
    fn command_word_help(&self) -> BTreeMap<String, String> {
        BTreeMap::from([("asset".to_owned(), "cached help\n".to_owned())])
    }
    fn run(&self, call: RunCall, streams: Streams, _: &TreeContext) -> RunResult {
        self.0.lock().push(call.argv);
        render_command(streams, "", String::new(), 2)
    }
    fn invoke(&self, _: CommandProposal, _: Streams, _: &TreeContext) -> CapabilityCallResult {
        panic!("Run owns dispatch")
    }
}

#[test]
fn ifs_never_splits_a_reference() {
    let commands = Commands::default();
    let outcome = Interpreter::new(Limits::default()).run(
        "IFS='chat-asset:3'; asset chat-asset:3 'chat-asset:3' --image=chat-asset:3",
        &commands,
    );
    assert_eq!(outcome.exit_code.get(), 2);
    assert_eq!(
        *commands.0.lock(),
        [vec![
            Arg::Asset(dekopon_core::AssetNumber::parse("chat-asset:3").unwrap()),
            Arg::Text("chat-asset:3".to_owned()),
            Arg::Text("--image=chat-asset:3".to_owned())
        ]]
    );
}

#[test]
fn a_reference_outside_argv_expands_to_its_text() {
    let commands = Commands::default();
    let outcome = Interpreter::new(Limits::default()).run("x=chat-asset:3; echo $x; for y in chat-asset:3; do echo $y; done; case chat-asset:3 in chat-asset:3) echo yes;; esac; printf '%s' chat-asset:3", &commands);
    assert_eq!(outcome.exit_code.get(), 0);
    assert_eq!(
        outcome.output,
        "chat-asset:3\nchat-asset:3\nyes\nchat-asset:3"
    );
    assert!(commands.0.lock().is_empty());
}

#[test]
fn a_builtin_names_asset_cat_for_a_reference_operand() {
    for command in ["head", "cat"] {
        let outcome = Interpreter::new(Limits::default()).run(
            &format!("echo buffered > chat-asset:3; {command} chat-asset:3"),
            &Commands::default(),
        );
        assert_eq!(
            outcome.output,
            format!(
                "{command}: chat-asset:3 is an attachment, not a buffer; read it with `asset cat chat-asset:3 | {command}`"
            )
        );
    }
}

#[test]
fn a_literal_help_sends_no_frame() {
    let commands = Commands::default();
    let outcome = Interpreter::new(Limits {
        max_capability_calls: 0,
        ..Limits::default()
    })
    .run("asset --help", &commands);
    assert_eq!(outcome.exit_code.get(), 0);
    assert_eq!(outcome.output, "cached help");
    assert!(commands.0.lock().is_empty());
}

#[test]
fn a_usage_error_refunds_its_call() {
    let commands = Commands::default();
    let calls = CallBudget::new(1);
    let tree = TreeContext::new(Limits::default(), calls.clone());
    let outcome = Interpreter::new(Limits::default()).run_with_tree(
        "asset invalid; asset invalid",
        &commands,
        &tree,
    );
    assert_eq!(outcome.exit_code.get(), 2);
    assert_eq!(commands.0.lock().len(), 2);
    assert_eq!(calls.used(), 0);
}

#[test]
fn concurrent_stages_cannot_overrun_the_call_budget() {
    let calls = CallBudget::new(1);
    std::thread::scope(|scope| {
        let handles = (0..16)
            .map(|_| scope.spawn(|| calls.charge().is_ok()))
            .collect::<Vec<_>>();
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|admitted| *admitted)
                .count(),
            1
        );
    });
    assert_eq!(calls.used(), 1);
}
