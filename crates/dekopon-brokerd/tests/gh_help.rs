#![allow(clippy::disallowed_methods)]
use std::collections::BTreeMap;

use dekopon_broker_host::{BrokerHostLimits, BrokerProviderRegistry, CommandRunOutcome};
use dekopon_process::{Limits, TreeContext};
use dekopon_shell::{
    CapabilityCallResult, CapabilityInvoker, CommandProposal, Interpreter, RunCall, RunResult,
    Streams, render_command,
};
use dekopon_test_support::provider_fixture;

struct HelpHost {
    registry: BrokerProviderRegistry,
    runtime: tokio::runtime::Runtime,
}

impl CapabilityInvoker for HelpHost {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }

    fn command_words(&self) -> Vec<String> {
        self.registry.command_words()
    }

    fn command_word_help(&self) -> BTreeMap<String, String> {
        self.registry.command_word_help()
    }

    fn run(&self, call: RunCall, streams: Streams, _tree: &TreeContext) -> RunResult {
        let argv = call
            .argv
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert_eq!(call.word, "gh");
        assert_eq!(argv, ["pr", "--help"], "root help must use the host cache");
        let outcome = self
            .runtime
            .block_on(self.registry.run_command(&call.word, &argv, false))
            .expect("released gh help runs without host imports");
        match outcome {
            CommandRunOutcome::Rendered {
                stdout,
                stderr,
                status,
            } => render_command(streams, &stdout, stderr, status),
            other @ (CommandRunOutcome::Proposed { .. } | CommandRunOutcome::Failed { .. }) => {
                panic!("help must render without proposing effects: {other:?}");
            }
        }
    }

    fn invoke(
        &self,
        _proposal: CommandProposal,
        _streams: Streams,
        _tree: &TreeContext,
    ) -> CapabilityCallResult {
        panic!("help must not invoke a capability or make an HTTP call")
    }
}

#[test]
fn the_shell_serves_the_released_gh_root_help_whole() {
    let path = provider_fixture("gh-provider.wasm");
    std::fs::metadata(&path).expect("fetch the released gh fixture before running this test");
    let runtime = tokio::runtime::Runtime::new().expect("host runtime");
    let registry = runtime
        .block_on(BrokerProviderRegistry::load(
            [path],
            BrokerHostLimits::default(),
        ))
        .expect("load released gh component through the broker host");
    let root = runtime
        .block_on(registry.run_command("gh", &["--help".to_owned()], false))
        .expect("component root help");
    let CommandRunOutcome::Rendered {
        stdout: root,
        stderr,
        status,
    } = root
    else {
        panic!("component root help must render");
    };
    assert_eq!(status, 0);
    assert!(stderr.is_empty());
    let host = HelpHost { registry, runtime };
    let shell = Interpreter::new(Limits::default(), dekopon_test_support::shell_threads());
    let help = shell.run("gh --help", &host);
    assert_eq!(help.exit_code.get(), 0, "{}", help.output);
    assert_eq!(help.output, root.strip_suffix('\n').unwrap_or(&root));
    assert_eq!(host.registry.command_word_help().get("gh"), Some(&root));
    assert!(!help.output.contains("[--help truncated"));
    assert!(root.len() <= 2048, "root help is {} bytes", root.len());
    assert!(
        root.contains("gh GROUP --help") && root.contains("  pr "),
        "drill-down is discoverable: {root}"
    );
    let drill_down = shell.run("gh pr --help", &host);
    assert_eq!(drill_down.exit_code.get(), 0, "{}", drill_down.output);
    assert!(
        drill_down.output.contains("pr checkout"),
        "{}",
        drill_down.output
    );
    assert!(
        drill_down.output.contains("needs a local git checkout"),
        "{}",
        drill_down.output
    );
    println!(
        "released gh root: {} bytes; cache equals component root byte-for-byte; shell display omits its terminal LF; pr checkout exclusion and reason retained",
        root.len()
    );
}
