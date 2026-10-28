#![allow(
    clippy::disallowed_methods,
    clippy::unwrap_used,
    reason = "tests spawn, join and build runtimes freely; production sites carry their own expectation"
)]
use dekopon_process::Limits;
use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, Interpreter};

fn shell_threads() -> dekopon_process::ShellThreads {
    static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    });
    dekopon_process::ShellThreads::new(16, RUNTIME.handle().clone())
}

struct Invoker;
impl CapabilityInvoker for Invoker {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }
    fn invoke(
        &self,
        _: dekopon_shell::CommandProposal,
        _streams: dekopon_shell::Streams,
        _tree: &dekopon_process::TreeContext,
    ) -> CapabilityCallResult {
        CapabilityCallResult::NotFound
    }
}

#[test]
fn a_worker_without_an_executable_fails_the_stage() {
    let outcome =
        Interpreter::new(Limits::default(), shell_threads()).run("jq .; echo alive", &Invoker);
    assert_eq!(outcome.exit_code.get(), 0, "{outcome:?}");
    assert!(
        outcome.output.contains("no worker executable supplied"),
        "{outcome:?}"
    );
    assert!(outcome.output.ends_with("alive"), "{outcome:?}");
}
