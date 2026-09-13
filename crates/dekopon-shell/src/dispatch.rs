//! Command-word resolution.
//!
//! Resolution order is fixed:
//!
//! 1. words this shell refuses outright (`eval`, `exec`, `source`, job control, `declare`),
//! 2. shell functions declared earlier in the same script,
//! 3. the fixed builtin table,
//! 4. command words declared by the session's loaded providers.
//!
//! Otherwise the word is "command not found", exit code 127. That includes a word shaped like a
//! capability identifier (`wikipedia_page`, `cli-probe.upper`), granted or not: a capability is
//! reached only through the provider command word that proposes it, never by typing its
//! identifier, so a granted capability's name is as unknown here as a typo.
//!
//! Step 4 could collide with step 3, and would lose. A provider word matching a builtin or any
//! other reserved word is refused at load by `dekopon_core::command_word_conflicts`, so a manifest
//! that would be shadowed never reaches this table; the ordering here is the second line.

use std::collections::BTreeSet;

use crate::{
    CapabilityInvoker,
    builtins::{self, BuiltinKind},
    parser::REJECTED_COMMANDS,
};

/// How one command word resolves.
pub(crate) enum Resolution {
    /// A shell function declared earlier in this script.
    Function,
    /// A builtin.
    Builtin(BuiltinKind),
    /// A command word a loaded provider contributed, rewritten by that provider into a proposal.
    ProviderCommand,
    /// A word this shell refuses, with the reason why.
    Rejected(&'static str),
    /// Nothing matched.
    NotFound,
}

/// Resolves one command word.
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
    // After builtins, so a provider can never shadow one. A provider claiming a builtin name is
    // refused at load rather than silently losing here; this ordering is the second line.
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

    /// A session granted two capability-shaped identifiers and one provider word, `probe`.
    ///
    /// It answers membership directly, counts every list it is asked to build, and records every
    /// capability it is asked to invoke.
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

    /// A session that only lists its command words, so membership goes through the trait default.
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
        // The query runs for every command word a script executes, so a loop of a thousand
        // commands used to build, sort, and dedup the whole command-word list a thousand times
        // over. An invoker that can answer directly must never be asked for it.
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
        // `Words` does not override `has_command_word`, so it falls back to scanning. An embedder
        // that has nothing cheaper must still resolve identically.
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

    /// `dekopon_core::RESERVED_COMMAND_WORDS` and this crate's live tables must agree exactly.
    ///
    /// The list lives in `dekopon-core` so the broker can report command-word conflicts at its own
    /// startup without linking an interpreter it never runs. That mirroring is only safe while it
    /// is checked, and it has to be checked in **both** directions.
    ///
    /// Missing a word means a provider could claim something the shell would then shadow, and the
    /// manifest would be a lie. Reserving a word no table owns is the opposite failure and the one
    /// worth naming: `gh` was reserved until its builtin was deleted, and had the entry outlived
    /// the builtin it would have kept the out-of-tree `gh` provider from claiming its own name.
    #[test]
    fn the_reserved_list_matches_the_shells_own_tables() {
        let live = builtins::names()
            .into_iter()
            .chain(CONTROL_WORDS.iter().copied())
            .chain(REJECTED_COMMANDS.iter().map(|(word, _)| *word))
            // Grammar keywords belong here for the same reason: the parser consumes them before
            // dispatch ever runs, so a provider declaring `do` as a command word would load
            // successfully and then never be reachable — a manifest that is a lie.
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
