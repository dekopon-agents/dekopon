//! The typed provider contract: a provider declares its capabilities once, and the manifest, input
//! schemas, help, argv parsing and dispatch are derived from those declarations.

use std::borrow::Cow;
use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;

use clap::{CommandFactory, FromArgMatches};
use dekopon_core::IdentifierError;
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    CapabilityId, CommandRunOutcome, ComponentFailure, ComponentResponse, EffectKind,
    ProviderApiVersion, ProviderCapability, ProviderId, ProviderManifest, RiskLevel,
    SecretUseProposal,
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
    /// authorization, so it is pure and reaches no import.
    fn propose(args: Self::Args, stdin: Option<&str>) -> Result<Proposal<Self>, Usage>;
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
    /// The value serialized into a successful response.
    type Output: Serialize;
    /// The capability's own failures.
    type Error: Failure;

    /// Runs one authorized call; a failure is reported with its [`Code`] and display text.
    fn run(input: Self::Input, needs: Self::Needs) -> Result<Self::Output, Self::Error>;
}

/// The imports a capability is granted; `()` grants none.
pub trait Needs: sealed::Needs {}

impl Needs for () {}

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

/// A failure code: lowercase ASCII letters and digits in hyphen-separated words.
///
/// ```compile_fail
/// const BAD: dekopon_provider_sdk::provider::Code =
///     dekopon_provider_sdk::provider::Code::new("Not Kebab");
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Code(&'static str);

impl Code {
    /// The capability id names no capability the provider lists.
    pub const UNKNOWN_CAPABILITY: Self = Self::new("unknown-capability");
    /// The input does not match the capability's input type.
    pub const INVALID_INPUT: Self = Self::new("invalid-input");
    /// The operator's settings do not match the capability's settings type.
    pub const INVALID_SETTINGS: Self = Self::new("invalid-settings");
    /// A provider declined parsed arguments.
    pub const USAGE: Self = Self::new("usage");
    /// The output or proposal input could not be serialized.
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
        Self(code)
    }

    /// The code as it appears on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
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
    /// The output or proposal input could not be serialized.
    SerializationFailed,
}

impl fmt::Display for SdkFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnknownCapability => "the provider has no such capability",
            Self::InvalidInput => "the input does not match the capability's input schema",
            Self::InvalidSettings => "the provider settings do not match their schema",
            Self::SerializationFailed => "the provider could not serialize its result",
        })
    }
}

impl Failure for SdkFailure {
    fn code(&self) -> Code {
        match self {
            Self::UnknownCapability => Code::UNKNOWN_CAPABILITY,
            Self::InvalidInput => Code::INVALID_INPUT,
            Self::InvalidSettings => Code::INVALID_SETTINGS,
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
pub trait Capabilities<P: Provider>: sealed::Capabilities<P> {}

mod sealed {
    use super::{Capability, ComponentResponse, IdentifierError, Provider, ProviderCapability};

    pub trait Needs: Sized {
        fn grant() -> Self;
    }

    impl Needs for () {
        fn grant() -> Self {}
    }

    pub trait Capabilities<P: Provider> {
        fn describe(provider: &str) -> Result<Vec<ProviderCapability>, IdentifierError>;
        fn lists(name: &str) -> bool;
        fn call(name: &str, input: &str) -> Option<ComponentResponse>;
    }

    macro_rules! tuple {
        ($($capability:ident),+) => {
            impl<P: Provider, $($capability: Capability<Provider = P>),+> Capabilities<P>
                for ($($capability,)+)
            {
                fn describe(provider: &str) -> Result<Vec<ProviderCapability>, IdentifierError> {
                    Ok(vec![$(super::describe::<$capability>(provider)?),+])
                }

                fn lists(name: &str) -> bool {
                    $(name == $capability::NAME)||+
                }

                fn call(name: &str, input: &str) -> Option<ComponentResponse> {
                    $(if name == $capability::NAME {
                        return Some(super::run::<$capability>(input));
                    })+
                    None
                }
            }

            impl<P: Provider, $($capability: Capability<Provider = P>),+> super::Capabilities<P>
                for ($($capability,)+)
            {
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

fn describe<C: Capability>(provider: &str) -> Result<ProviderCapability, IdentifierError> {
    Ok(ProviderCapability {
        id: capability_id(provider, C::NAME)?,
        description: C::DESCRIPTION.to_owned(),
        effect: C::EFFECT,
        risk: C::RISK,
        input_schema: input_schema::<C::Input>(),
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

fn failure(failure: &impl Failure) -> ComponentFailure {
    ComponentFailure {
        code: failure.code().as_str().to_owned(),
        message: failure.to_string(),
    }
}

fn run<C: Capability>(input: &str) -> ComponentResponse {
    let Ok(input) = serde_json::from_str::<C::Input>(input) else {
        return ComponentResponse::Failed {
            error: failure(&SdkFailure::InvalidInput),
        };
    };
    match C::run(input, sealed::Needs::grant()) {
        Ok(output) => match serde_json::to_value(output) {
            Ok(output) => ComponentResponse::Succeeded { output },
            Err(_) => ComponentResponse::Failed {
                error: failure(&SdkFailure::SerializationFailed),
            },
        },
        Err(error) => ComponentResponse::Failed {
            error: failure(&error),
        },
    }
}

/// The manifest derived from `P`'s declarations, or the error naming an invalid identifier.
pub fn manifest<P: Provider>() -> Result<ProviderManifest, IdentifierError> {
    Ok(ProviderManifest {
        api_version: ProviderApiVersion::V1Alpha1,
        id: P::ID.parse::<ProviderId>()?,
        description: P::DESCRIPTION.to_owned(),
        capabilities: <P::Capabilities as sealed::Capabilities<P>>::describe(P::ID)?,
        command_words: P::COMMAND_WORDS
            .iter()
            .map(|&word| word.to_owned())
            .collect(),
    })
}

/// Runs one authorized call of the capability `capability` names with the JSON `input`.
#[must_use]
pub fn call<P: Provider>(capability: &str, input: &str) -> ComponentResponse {
    capability
        .strip_prefix(P::ID)
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|name| <P::Capabilities as sealed::Capabilities<P>>::call(name, input))
        .unwrap_or_else(|| ComponentResponse::Failed {
            error: failure(&SdkFailure::UnknownCapability),
        })
}

/// Parses `argv`, the words after the command word, and renders help or a usage error or
/// returns the provider's proposal.
#[must_use]
pub fn command<P: Provider>(argv: &[String], stdin: Option<&str>) -> CommandRunOutcome {
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
        Ok(args) => match P::propose(args, stdin) {
            Ok(proposal) => proposed(proposal),
            Err(Usage(message)) => CommandRunOutcome::Failed {
                error: ComponentFailure {
                    code: Code::USAGE.as_str().to_owned(),
                    message: message.into_owned(),
                },
            },
        },
        Err(error) => {
            let text = error.render().to_string();
            if error.use_stderr() {
                CommandRunOutcome::Rendered {
                    stdout: String::new(),
                    stderr: text,
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
