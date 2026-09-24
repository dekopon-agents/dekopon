//! A command word shaped like a capability identifier is never resolved directly, granted or not,
//! since a capability is only reachable through the provider command word that proposes it, and a
//! provider word colliding with a builtin is refused when the provider loads rather than silently
//! shadowed here.

use std::collections::BTreeSet;

use crate::{
    CapabilityInvoker,
    builtins::{self, BuiltinKind},
    parser::REJECTED_COMMANDS,
};

pub(crate) enum Resolution {
    Function,
    Builtin(BuiltinKind),
    ProviderCommand,
    Rejected(&'static str),
    NotFound,
}

pub(crate) fn resolve(
    word: &str,
    functions: &BTreeSet<String>,
    invoker: &dyn CapabilityInvoker,
) -> Resolution {
    if let Some((_, reason)) = REJECTED_COMMANDS
        .iter()
        .find(|(rejected, _)| *rejected == word)
    {
        return Resolution::Rejected(reason);
    }
    if functions.contains(word) {
        return Resolution::Function;
    }
    if let Some(builtin) = builtins::lookup(word) {
        return Resolution::Builtin(builtin);
    }
    if invoker.has_command_word(word) {
        return Resolution::ProviderCommand;
    }
    Resolution::NotFound
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeSet,
    };

    use dekopon_core::SecretUseProposal;
    use serde_json::Value;

    use crate::{CapabilityCallResult, CapabilityInvoker, ExitCode, Interpreter, Limits};

    use super::{Resolution, resolve};

    #[derive(Default)]
    struct Session {
        granted_lists: Cell<usize>,
        word_lists: Cell<usize>,
        invoked: RefCell<Vec<String>>,
    }

    impl CapabilityInvoker for Session {
        fn granted(&self) -> Vec<String> {
            self.granted_lists.set(self.granted_lists.get() + 1);
            vec!["cli-probe.upper".to_owned(), "wikipedia_page".to_owned()]
        }

        fn is_granted(&self, capability: &str) -> bool {
            matches!(capability, "cli-probe.upper" | "wikipedia_page")
        }

        fn command_words(&self) -> Vec<String> {
            self.word_lists.set(self.word_lists.get() + 1);
            vec!["probe".to_owned()]
        }

        fn has_command_word(&self, word: &str) -> bool {
            word == "probe"
        }

        fn invoke(
            &self,
            capability: &str,
            input: Value,
            secret_use: Option<SecretUseProposal>,
        ) -> CapabilityCallResult {
            if secret_use.is_some() {
                return crate::secret_use_unsupported();
            }
            self.invoked.borrow_mut().push(capability.to_owned());
            CapabilityCallResult::Succeeded(input)
        }
    }

    struct Words(&'static [&'static str]);

    impl CapabilityInvoker for Words {
        fn granted(&self) -> Vec<String> {
            Vec::new()
        }

        fn command_words(&self) -> Vec<String> {
            self.0.iter().map(|word| (*word).to_owned()).collect()
        }

        fn invoke(
            &self,
            _capability: &str,
            _input: Value,
            secret_use: Option<SecretUseProposal>,
        ) -> CapabilityCallResult {
            if secret_use.is_some() {
                return crate::secret_use_unsupported();
            }
            CapabilityCallResult::NotFound
        }
    }

    #[test]
    fn resolution_follows_the_documented_priority_order() {
        let mut functions = BTreeSet::new();
        functions.insert("greet".to_owned());
        let session = Session::default();

        assert!(matches!(
            resolve("eval", &functions, &session),
            Resolution::Rejected(_)
        ));
        assert!(matches!(
            resolve("greet", &functions, &session),
            Resolution::Function
        ));
        assert!(matches!(
            resolve("jq", &functions, &session),
            Resolution::Builtin(_)
        ));
        assert!(matches!(
            resolve("probe", &functions, &session),
            Resolution::ProviderCommand
        ));
        assert!(matches!(
            resolve("unknown", &functions, &session),
            Resolution::NotFound
        ));
    }

    #[test]
    fn a_granted_capability_identifier_is_not_a_command_word() {
        let functions = BTreeSet::new();
        let session = Session::default();
        for word in ["wikipedia_page", "cli-probe.upper", "cli-probe.count"] {
            assert!(
                matches!(resolve(word, &functions, &session), Resolution::NotFound),
                "{word} resolved to something other than command not found"
            );
        }
    }

    #[test]
    fn a_capability_shaped_word_exits_127_with_the_ordinary_not_found_message() {
        let session = Session::default();
        for (script, word) in [
            ("wikipedia_page --title x", "wikipedia_page"),
            ("cli-probe.upper --text x", "cli-probe.upper"),
        ] {
            let outcome = Interpreter::new(Limits::default()).run(script, &session);
            assert_eq!(outcome.exit_code, ExitCode::NOT_FOUND, "{script}");
            assert_eq!(
                outcome.output,
                format!("dekopon-shell: {word}: command not found"),
                "{script}"
            );
            assert_eq!(outcome.capability_calls, 0, "{script}");
        }
        assert!(
            session.invoked.borrow().is_empty(),
            "a capability identifier typed as a command reached invoke"
        );
    }

    #[test]
    fn resolution_asks_membership_instead_of_materializing_a_list_per_command() {
        let session = Session::default();
        let functions = BTreeSet::new();
        for word in ["probe", "wikipedia_page", "cli-probe.upper", "unknown"] {
            let _resolution = resolve(word, &functions, &session);
        }
        assert_eq!(session.granted_lists.get(), 0);
        assert_eq!(session.word_lists.get(), 0);
    }

    #[test]
    fn the_default_membership_query_answers_from_the_list() {
        let functions = BTreeSet::new();
        let words = Words(&["probe"]);
        assert!(matches!(
            resolve("probe", &functions, &words),
            Resolution::ProviderCommand
        ));
        assert!(matches!(
            resolve("prob", &functions, &words),
            Resolution::NotFound
        ));
    }

    #[test]
    fn a_builtin_wins_over_a_provider_word_and_a_function_over_both() {
        let mut functions = BTreeSet::new();
        let words = Words(&["jq", "probe"]);
        assert!(matches!(
            resolve("jq", &functions, &words),
            Resolution::Builtin(_)
        ));
        functions.insert("jq".to_owned());
        functions.insert("probe".to_owned());
        assert!(matches!(
            resolve("jq", &functions, &words),
            Resolution::Function
        ));
        assert!(matches!(
            resolve("probe", &functions, &words),
            Resolution::Function
        ));
    }
}

#[cfg(test)]
mod reserved {
    use std::collections::BTreeSet;

    use crate::{
        builtins,
        interp::telemetry::CONTROL_WORDS,
        parser::{REJECTED_COMMANDS, RESERVED_WORDS},
    };

    /// This crate's word tables and the core crate's reserved command word list must agree in both
    /// directions, or a provider could claim a name the shell secretly shadows, or an unused
    /// reservation could block a provider's own name forever.
    #[test]
    fn the_reserved_list_matches_the_shells_own_tables() {
        let live = builtins::names()
            .into_iter()
            .chain(CONTROL_WORDS.iter().copied())
            .chain(REJECTED_COMMANDS.iter().map(|(word, _)| *word))
            .chain(RESERVED_WORDS.iter().copied())
            .collect::<BTreeSet<_>>();
        let declared = dekopon_core::RESERVED_COMMAND_WORDS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();

        let missing = live.difference(&declared).collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "dekopon_core::RESERVED_COMMAND_WORDS is missing {missing:?}; a provider could claim \
             one and be silently shadowed"
        );
        let stale = declared.difference(&live).collect::<Vec<_>>();
        assert!(
            stale.is_empty(),
            "dekopon_core::RESERVED_COMMAND_WORDS reserves {stale:?}, which no table owns; a \
             provider that should be able to claim one cannot"
        );
    }
}
