use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, Interpreter, Limits};
use serde_json::Value;

struct Invoker;
impl CapabilityInvoker for Invoker {
    fn granted(&self) -> Vec<String> {
        Vec::new()
    }
    fn invoke(
        &self,
        _: &str,
        _: Value,
        _: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        CapabilityCallResult::NotFound
    }
}

#[test]
fn a_worker_without_an_executable_fails_the_stage() {
    let outcome = Interpreter::new(Limits::default()).run("jq .; echo alive", &Invoker);
    assert_eq!(outcome.exit_code.get(), 0, "{outcome:?}");
    assert!(
        outcome.output.contains("no worker executable supplied"),
        "{outcome:?}"
    );
    assert!(outcome.output.ends_with("alive"), "{outcome:?}");
}
