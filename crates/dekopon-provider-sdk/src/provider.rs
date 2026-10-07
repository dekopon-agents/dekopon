mod bounded;
mod handles;
#[cfg(not(target_arch = "wasm32"))]
mod port;
mod stdio;
pub use crate::http::{
    BuildError as HttpBuildError, Header, HttpError, HttpErrorCode, Part, Request, Response,
    StreamedRequest, StreamedResponse, method,
};
pub use crate::spawn::{Child, ChildStdin, Exit, SpawnError};
pub use crate::storage::{durable_files, jsonl};
pub use bounded::{Bounded, TooLong, Truncated};
pub use handles::{
    Assets, Clock, DurableFiles, Http, Jsonl, Monotonic, Random, Settings, Spawn, Storage,
};
#[cfg(not(target_arch = "wasm32"))]
pub use port::{NativeChild, NativeChildStdin, Port, with_port};
#[cfg(not(target_arch = "wasm32"))]
pub use stdio::{NativeExit, NativeStdio, invoke_native};
pub use stdio::{Stdin, Stdout, stdin};

use std::borrow::Cow;
use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU8;

use clap::{CommandFactory, FromArgMatches};
use dekopon_core::IdentifierError;
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    CapabilityId, CommandRunOutcome, ComponentFailure, EffectKind, ProviderApiVersion,
    ProviderCapability, ProviderId, ProviderManifest, RiskLevel, SecretUseProposal,
};

/// A provider: its identity, command words, argv grammar and the capabilities it exports.
pub trait Provider: Sized + 'static {
    /// The provider identifier; each capability id is derived as `ID.NAME`.
    const ID: &'static str;
    /// The words that reach this provider from a model's script.
    const COMMAND_WORDS: &'static [&'static str];
    /// The operator- and model-facing description.
    const DESCRIPTION: &'static str;
    /// The argv grammar; help, version and usage errors are rendered from it.
    type Args: clap::Parser;
    /// The exported capabilities, as a tuple of up to 19.
    type Capabilities: Capabilities<Self>;

    /// Maps parsed arguments to one capability proposal or a usage error; it runs before
    /// authorization, so it is pure and reaches no import. `stdin_piped` says whether input is
    /// piped in; the input itself is read by the capability through [`stdin`].
    fn propose(args: Self::Args, stdin_piped: bool) -> Result<Proposal<Self>, Usage>;
}

/// One capability of a provider.
pub trait Capability: Sized + 'static {
    /// The provider that lists this capability.
    type Provider: Provider;
    /// The name after the provider id in the capability id.
    const NAME: &'static str;
    /// The model- and operator-facing description.
    const DESCRIPTION: &'static str;
    /// Effect classification.
    const EFFECT: EffectKind;
    /// Coarse risk classification.
    const RISK: RiskLevel;
    /// The input; its JSON Schema is the capability's `inputSchema`.
    type Input: DeserializeOwned + Serialize + JsonSchema;
    /// The imports the capability is granted for one authorized call.
    type Needs: Needs;
    /// The capability's own failures.
    type Error: Failure;

    /// Runs one authorized call, writing its output to `out`; a failure exits with its
    /// [`Code`]'s status and its display text on stderr.
    fn run(input: Self::Input, needs: Self::Needs, out: &mut Stdout) -> Result<(), Self::Error>;
}

/// The imports a capability is granted; `()` grants none.
///
/// ```compile_fail
/// use dekopon_provider_sdk::provider::Http;
/// let unauthorized = Http { private: () };
/// ```
pub trait Needs: sealed::Needs {
    /// The import interfaces required by this need.
    const IMPORTS: ImportSet;
}

impl Needs for () {
    const IMPORTS: ImportSet = ImportSet::EMPTY;
}

/// A set of guest import interfaces declared by a capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportSet(u16);

impl ImportSet {
    /// No guest imports.
    pub const EMPTY: Self = Self(0);
    /// Buffered and asset-backed HTTP requests.
    pub const HTTP: Self = Self(1);
    /// Broker wall clock.
    pub const CLOCK: Self = Self(2);
    /// Per-provider settings.
    pub const SETTINGS: Self = Self(4);
    /// JSONL storage.
    pub const JSONL: Self = Self(8);
    /// Durable file storage.
    pub const DURABLE_FILES: Self = Self(16);
    /// Conversation assets.
    pub const ASSETS: Self = Self(32);
    /// Invocation-relative monotonic time.
    pub const MONOTONIC: Self = Self(64);
    /// OS entropy.
    pub const RANDOM: Self = Self(128);
    /// Child shell scripts run by the gateway.
    pub const SPAWN: Self = Self(256);

    /// Combines two declarations without granting either one.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether all imports in `other` were declared.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

macro_rules! need_tuple {
    ($($need:ident),+) => {
        impl<$($need: Needs),+> sealed::Needs for ($($need,)+) {
            fn grant() -> Result<Self, SdkFailure> {
                Ok(($(<$need as sealed::Needs>::grant()?,)+))
            }
        }
        impl<$($need: Needs),+> Needs for ($($need,)+) {
            const IMPORTS: ImportSet = ImportSet::EMPTY$(.union($need::IMPORTS))+;
        }
    };
}
macro_rules! need_tuples {
    ($head:ident $(, $tail:ident)*) => {
        need_tuple!($head $(, $tail)*);
        need_tuples!($($tail),*);
    };
    () => {};
}
need_tuples!(
    N19, N18, N17, N16, N15, N14, N13, N12, N11, N10, N9, N8, N7, N6, N5, N4, N3, N2, N1
);

/// A capability failure reported to the model: a stable code and its display text.
pub trait Failure: fmt::Display {
    /// The stable machine code for this failure.
    fn code(&self) -> Code;
}

impl Failure for Infallible {
    fn code(&self) -> Code {
        match *self {}
    }
}

/// A failure code: lowercase ASCII letters and digits in hyphen-separated words, and the exit
/// status a capability failing with it reports, 1 unless the code names its own.
///
/// ```compile_fail
/// const BAD: dekopon_provider_sdk::provider::Code =
///     dekopon_provider_sdk::provider::Code::new("Not Kebab");
/// ```
///
/// ```compile_fail
/// const ZERO: dekopon_provider_sdk::provider::Code =
///     dekopon_provider_sdk::provider::Code::new("zero").exiting(0);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Code {
    name: &'static str,
    status: NonZeroU8,
}

impl Code {
    /// The capability id names no capability the provider lists.
    pub const UNKNOWN_CAPABILITY: Self = Self::new("unknown-capability");
    /// The input does not match the capability's input type; a usage error.
    pub const INVALID_INPUT: Self = Self::new("invalid-input").exiting(2);
    /// The operator's settings do not match the capability's settings type.
    pub const INVALID_SETTINGS: Self = Self::new("invalid-settings");
    /// This import requires execution through the real component host.
    pub const COMPONENT_HARNESS_REQUIRED: Self = Self::new("component-harness-required");
    /// A usage error, exit status 2: declined arguments, or piped input that is required but empty.
    pub const USAGE: Self = Self::new("usage").exiting(2);
    /// The proposal input could not be serialized.
    pub const SERIALIZATION_FAILED: Self = Self::new("serialization-failed");

    /// Panics unless `code` is hyphen-separated lowercase words, so an invalid code in a `const`
    /// is a compile error.
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        let bytes = code.as_bytes();
        assert!(
            !bytes.is_empty() && bytes[0] != b'-' && bytes[bytes.len() - 1] != b'-',
            "a code is hyphen-separated lowercase words"
        );
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            assert!(
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || (byte == b'-' && bytes[index - 1] != b'-'),
                "a code is hyphen-separated lowercase words"
            );
            index += 1;
        }
        Self {
            name: code,
            status: NonZeroU8::MIN,
        }
    }

    /// The same code exiting with `status`; panics on 0, so a zero status in a `const` is a
    /// compile error.
    #[must_use]
    pub const fn exiting(self, status: u8) -> Self {
        let Some(status) = NonZeroU8::new(status) else {
            panic!("a failure exits with a nonzero status");
        };
        Self { status, ..self }
    }

    /// The code as it appears on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.name
    }

    /// The exit status of an invocation failing with this code.
    #[must_use]
    pub const fn status(self) -> NonZeroU8 {
        self.status
    }
}

/// The failures the SDK reports itself, with static messages that never echo the input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SdkFailure {
    /// The capability id names no capability the provider lists.
    UnknownCapability,
    /// The input does not match the capability's input type.
    InvalidInput,
    /// The operator's settings do not match the capability's settings type.
    InvalidSettings,
    /// Native execution cannot supply storage or asset imports.
    ComponentHarnessRequired,
    /// The proposal input could not be serialized.
    SerializationFailed,
}

impl fmt::Display for SdkFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnknownCapability => "the provider has no such capability",
            Self::InvalidInput => "the input does not match the capability's input schema",
            Self::InvalidSettings => "the provider settings do not match their schema",
            Self::ComponentHarnessRequired => {
                "this capability needs the component harness (Harness<P>)"
            }
            Self::SerializationFailed => "the provider could not serialize its proposal",
        })
    }
}

impl Failure for SdkFailure {
    fn code(&self) -> Code {
        match self {
            Self::UnknownCapability => Code::UNKNOWN_CAPABILITY,
            Self::InvalidInput => Code::INVALID_INPUT,
            Self::InvalidSettings => Code::INVALID_SETTINGS,
            Self::ComponentHarnessRequired => Code::COMPONENT_HARNESS_REQUIRED,
            Self::SerializationFailed => Code::SERIALIZATION_FAILED,
        }
    }
}

/// A provider's refusal of parsed arguments, reported to the model as a usage error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Usage(Cow<'static, str>);

impl Usage {
    /// A usage error with `message` shown to the model.
    #[must_use]
    pub fn new(message: impl Into<Cow<'static, str>>) -> Self {
        Self(message.into())
    }
}

/// One capability proposal; it grants nothing, since the broker authorizes it separately.
pub struct Proposal<P: Provider> {
    capability: &'static str,
    input: Result<Value, serde_json::Error>,
    secret_use: Option<SecretUseProposal>,
    provider: PhantomData<fn() -> P>,
}

impl<P: Provider> Proposal<P> {
    /// A proposal to run `C` with `input`, naming no secret use.
    #[must_use]
    pub fn to<C: Capability<Provider = P>>(input: C::Input) -> Self {
        Self {
            capability: C::NAME,
            input: serde_json::to_value(input),
            secret_use: None,
            provider: PhantomData,
        }
    }

    /// The proposal naming a secret use, which is untrusted intent the broker authorizes.
    #[must_use]
    pub fn with_secret_use(self, secret_use: SecretUseProposal) -> Self {
        Self {
            secret_use: Some(secret_use),
            ..self
        }
    }
}

/// The capabilities a provider lists: a tuple of up to 19 [`Capability`] types.
pub trait Capabilities<P: Provider>: sealed::Capabilities<P> {
    /// The union of imports declared by the capabilities in this tuple.
    const IMPORTS: ImportSet;
}

mod sealed {
    use super::{Capability, ManifestError, Provider, ProviderCapability, Stdout, stdio::Exit};

    pub trait Needs: Sized {
        fn grant() -> Result<Self, super::SdkFailure>;
    }

    impl Needs for () {
        fn grant() -> Result<Self, super::SdkFailure> {
            Ok(())
        }
    }

    pub trait Capabilities<P: Provider> {
        fn describe(provider: &str) -> Result<Vec<ProviderCapability>, ManifestError>;
        fn lists(name: &str) -> bool;
        fn run(name: &str, input: &str, out: &mut Stdout) -> Option<Result<(), Exit>>;
    }

    macro_rules! tuple {
        ($($capability:ident),+) => {
            impl<P: Provider, $($capability: Capability<Provider = P>),+> Capabilities<P>
                for ($($capability,)+)
            {
                fn describe(provider: &str) -> Result<Vec<ProviderCapability>, ManifestError> {
                    Ok(vec![$(super::describe::<$capability>(provider)?),+])
                }

                fn lists(name: &str) -> bool {
                    $(name == $capability::NAME)||+
                }

                fn run(name: &str, input: &str, out: &mut Stdout) -> Option<Result<(), Exit>> {
                    $(if name == $capability::NAME {
                        return Some(super::run::<$capability>(input, out));
                    })+
                    None
                }
            }

            impl<P: Provider, $($capability: Capability<Provider = P>),+> super::Capabilities<P>
                for ($($capability,)+)
            {
                const IMPORTS: super::ImportSet = super::ImportSet::EMPTY$(.union(<$capability::Needs as super::Needs>::IMPORTS))+;
            }
        };
    }

    macro_rules! tuples {
        ($head:ident $(, $tail:ident)*) => {
            tuple!($head $(, $tail)*);
            tuples!($($tail),*);
        };
        () => {};
    }

    tuples!(
        C19, C18, C17, C16, C15, C14, C13, C12, C11, C10, C9, C8, C7, C6, C5, C4, C3, C2, C1
    );
}

fn capability_id(provider: &str, name: &str) -> Result<CapabilityId, IdentifierError> {
    format!("{provider}.{name}").parse()
}

fn describe<C: Capability>(provider: &str) -> Result<ProviderCapability, ManifestError> {
    let input_schema = input_schema::<C::Input>();
    if let Some(fault) = schema_fault(&input_schema) {
        return Err(ManifestError::Schema {
            capability: C::NAME,
            fault,
        });
    }
    Ok(ProviderCapability {
        id: capability_id(provider, C::NAME).map_err(ManifestError::Identifier)?,
        description: C::DESCRIPTION.to_owned(),
        effect: C::EFFECT,
        risk: C::RISK,
        input_schema,
    })
}

fn input_schema<T: JsonSchema>() -> Value {
    let mut settings = SchemaSettings::draft2020_12();
    settings.inline_subschemas = true;
    settings.meta_schema = None;
    let mut schema = settings.into_generator().into_root_schema_for::<T>();
    schema.remove("title");
    schema.to_value()
}

fn schema_fault(schema: &Value) -> Option<SchemaFault> {
    match schema {
        Value::Object(object) => {
            if object.contains_key("$ref") {
                return Some(SchemaFault::Reference);
            }
            let object_typed = match object.get("type") {
                Some(Value::String(kind)) => kind == "object",
                Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "object"),
                _ => false,
            };
            if object_typed && object.get("additionalProperties") != Some(&Value::Bool(false)) {
                return Some(SchemaFault::Open);
            }
            object
                .iter()
                .find_map(|(keyword, value)| match keyword.as_str() {
                    "default" | "examples" | "const" | "enum" => None,
                    "properties" | "patternProperties" => value
                        .as_object()
                        .and_then(|properties| properties.values().find_map(schema_fault)),
                    _ => schema_fault(value),
                })
        }
        Value::Array(items) => items.iter().find_map(schema_fault),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

fn failure(failure: &impl Failure) -> ComponentFailure {
    ComponentFailure {
        code: failure.code().as_str().to_owned(),
        message: failure.to_string(),
    }
}

fn run<C: Capability>(input: &str, out: &mut Stdout) -> Result<(), stdio::Exit> {
    let input = serde_json::from_str::<C::Input>(input)
        .map_err(|_invalid| stdio::Exit::from(&SdkFailure::InvalidInput))?;
    let needs = <C::Needs as sealed::Needs>::grant().map_err(|error| stdio::Exit::from(&error))?;
    C::run(input, needs, out).map_err(|error| stdio::Exit::from(&error))
}

/// Why an input schema cannot be published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaFault {
    /// An object schema accepts properties it does not list; use `#[serde(deny_unknown_fields)]`.
    Open,
    /// A subschema is a reference, which inlining could not resolve (a recursive type).
    Reference,
}

/// Why a manifest cannot be derived from a provider's declarations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestError {
    /// The provider id or a derived capability id is invalid.
    Identifier(IdentifierError),
    /// A capability's input schema is not closed and inline.
    Schema {
        /// The capability name.
        capability: &'static str,
        /// What is wrong with the schema.
        fault: SchemaFault,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identifier(error) => error.fmt(formatter),
            Self::Schema {
                capability,
                fault: SchemaFault::Open,
            } => write!(formatter, "the input schema of {capability} is not closed"),
            Self::Schema {
                capability,
                fault: SchemaFault::Reference,
            } => write!(
                formatter,
                "the input schema of {capability} has a reference"
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

/// The manifest derived from `P`'s declarations, or why it cannot be derived.
pub fn manifest<P: Provider>() -> Result<ProviderManifest, ManifestError> {
    Ok(ProviderManifest {
        api_version: ProviderApiVersion::V1Alpha1,
        id: P::ID
            .parse::<ProviderId>()
            .map_err(ManifestError::Identifier)?,
        description: P::DESCRIPTION.to_owned(),
        capabilities: <P::Capabilities as sealed::Capabilities<P>>::describe(P::ID)?,
        command_words: P::COMMAND_WORDS
            .iter()
            .map(|&word| word.to_owned())
            .collect(),
    })
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn invoke<P: Provider>(capability: &str, input: &str) -> Result<(), NonZeroU8> {
    let mut out = Stdout::open();
    let outcome = dispatch::<P>(capability, input, &mut out);
    stdio::exit(outcome, &out)
}

fn dispatch<P: Provider>(
    capability: &str,
    input: &str,
    out: &mut Stdout,
) -> Result<(), stdio::Exit> {
    capability
        .strip_prefix(P::ID)
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|name| <P::Capabilities as sealed::Capabilities<P>>::run(name, input, out))
        .unwrap_or_else(|| Err(stdio::Exit::from(&SdkFailure::UnknownCapability)))
}

/// Parses `argv`, the words after the command word, and renders help or a usage error or
/// returns the provider's proposal.
#[must_use]
pub fn command<P: Provider>(argv: &[String], stdin_piped: bool) -> CommandRunOutcome {
    let mut grammar = P::Args::command().no_binary_name(true);
    if grammar.get_bin_name().is_none() {
        let name = grammar.get_name().to_owned();
        grammar = grammar.bin_name(name);
    }
    let args = grammar
        .try_get_matches_from_mut(argv)
        .and_then(|matches| P::Args::from_arg_matches(&matches))
        .map_err(|error| error.format(&mut grammar));
    match args {
        Ok(args) => match P::propose(args, stdin_piped) {
            Ok(proposal) => proposed(proposal),
            Err(Usage(message)) => CommandRunOutcome::Failed {
                error: ComponentFailure {
                    code: Code::USAGE.as_str().to_owned(),
                    message: message.into_owned(),
                },
            },
        },
        Err(error) => {
            let use_stderr = error.use_stderr();
            let text = error.render().to_string();
            if use_stderr {
                let mut stderr = text;
                let help = matched_help(&grammar, argv);
                stderr.push_str(&bounded_help(&help, 4096));
                CommandRunOutcome::Rendered {
                    stdout: String::new(),
                    stderr,
                    status: u8::try_from(error.exit_code()).unwrap_or(2),
                }
            } else {
                CommandRunOutcome::Rendered {
                    stdout: text,
                    stderr: String::new(),
                    status: 0,
                }
            }
        }
    }
}

fn matched_help(grammar: &clap::Command, argv: &[String]) -> String {
    let mut command = grammar;
    let mut values = 0_usize;
    let mut options_done = false;
    for word in argv {
        if values > 0 {
            values -= 1;
            continue;
        }
        if word == "--" {
            options_done = true;
            continue;
        }
        if !options_done && word.starts_with('-') {
            let (name, mut inline) = word
                .split_once('=')
                .map_or((word.as_str(), false), |(name, _)| (name, true));
            let argument = if let Some(long) = name.strip_prefix("--") {
                command
                    .get_arguments()
                    .find(|arg| arg.get_long() == Some(long))
            } else if let Some(shorts) = name.strip_prefix('-') {
                let mut letters = shorts.chars();
                let argument = letters.next().and_then(|short| {
                    command
                        .get_arguments()
                        .find(|arg| arg.get_short() == Some(short))
                });
                if !letters.as_str().is_empty() {
                    if argument.is_some_and(|arg| {
                        arg.get_num_args()
                            .is_some_and(|range| range.min_values() > 0)
                    }) {
                        inline = true;
                    } else {
                        break;
                    }
                }
                argument
            } else {
                None
            };
            if let Some(argument) = argument {
                if !inline {
                    values = argument
                        .get_num_args()
                        .map_or(0, |range| range.min_values());
                }
            } else if name != "-h" && name != "--help" && name != "-V" && name != "--version" {
                break;
            }
            continue;
        }
        if let Some(next) = command.find_subcommand(word) {
            command = next;
        } else {
            break;
        }
    }
    let mut help = command.clone();
    help.render_long_help().to_string()
}

fn bounded_help(help: &str, maximum: usize) -> String {
    const MARKER: &str = "\n[help truncated]\n";
    if help.len() < maximum {
        return format!("\n{help}");
    }
    if maximum <= MARKER.len() + 1 {
        return String::new();
    }
    let mut end = maximum - MARKER.len() - 1;
    while !help.is_char_boundary(end) {
        end -= 1;
    }
    format!("\n{}{MARKER}", &help[..end])
}

fn proposed<P: Provider>(proposal: Proposal<P>) -> CommandRunOutcome {
    let refused = |reason: SdkFailure| CommandRunOutcome::Failed {
        error: failure(&reason),
    };
    if !<P::Capabilities as sealed::Capabilities<P>>::lists(proposal.capability) {
        return refused(SdkFailure::UnknownCapability);
    }
    let Ok(capability) = capability_id(P::ID, proposal.capability) else {
        return refused(SdkFailure::UnknownCapability);
    };
    let Ok(input) = proposal.input else {
        return refused(SdkFailure::SerializationFailed);
    };
    CommandRunOutcome::Proposed {
        capability,
        input,
        secret_use: proposal.secret_use,
    }
}

#[cfg(test)]
mod help_tests {
    use super::{bounded_help, matched_help};
    use clap::{Arg, Command};

    #[test]
    fn matched_help_ignores_option_values_and_uses_validated_nested_commands() {
        let grammar = Command::new("fixture")
            .arg(Arg::new("target").short('t').long("target").num_args(1))
            .subcommand(
                Command::new("count")
                    .about("Count items")
                    .subcommand(Command::new("deep").about("Deep count")),
            );
        let help = |words: &[&str]| {
            matched_help(
                &grammar,
                &words
                    .iter()
                    .map(|word| (*word).to_owned())
                    .collect::<Vec<_>>(),
            )
        };
        assert!(
            help(&["--target", "count", "--bad"]).contains("Usage: fixture"),
            "{}",
            help(&["--target", "count", "--bad"])
        );
        assert!(help(&["--target=count", "count", "deep", "--bad"]).contains("Deep count"));
        assert!(help(&["-tcount", "count", "deep", "--bad"]).contains("Deep count"));
        assert!(help(&["count", "--unknown", "deep"]).contains("Count items"));
        assert!(help(&["unknown", "count"]).contains("Usage: fixture"));
    }

    #[test]
    fn help_truncation_stays_on_utf8_boundaries_within_its_budget() {
        let help = "é".repeat(5000);
        let bounded = bounded_help(&help, 4096);
        assert!(bounded.len() <= 4096);
        assert!(bounded.ends_with("[help truncated]\n"));
    }
}
