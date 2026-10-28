use super::{MAX_ARITHMETIC_TOKENS, MAX_NESTING_DEPTH, nested::Construct};
use crate::{
    ast::Stream,
    lexer::{LexError, TokenKind},
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ParseError {
    Lex { line: usize, refusal: LexError },
    Refused { line: usize, refusal: Refusal },
    TooDeep { line: usize, refusal: TooDeep },
}

impl ParseError {
    pub(super) fn refused(line: usize, refusal: Refusal) -> Self {
        Self::Refused { line, refusal }
    }
    pub(crate) fn lexical(line: usize, refusal: LexError) -> Self {
        Self::Lex { line, refusal }
    }
    #[cfg(test)]
    pub(crate) fn line(&self) -> usize {
        match self {
            Self::Lex { line, .. } | Self::Refused { line, .. } | Self::TooDeep { line, .. } => {
                *line
            }
        }
    }
}
impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lex { line, refusal } => write!(formatter, "line {line}: {refusal}"),
            Self::Refused { line, refusal } => write!(formatter, "line {line}: {refusal}"),
            Self::TooDeep { line, refusal } => write!(formatter, "line {line}: {refusal}"),
        }
    }
}
impl std::error::Error for ParseError {}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Found(pub(super) Option<TokenKind>);
impl std::fmt::Display for Found {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(token) => token.fmt(formatter),
            None => formatter.write_str("end of script"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Refusal {
    UnexpectedToken {
        kind: TokenKind,
    },
    UnterminatedSubstitution,
    UnterminatedHereDocument,
    DuplicationOrder {
        source: Stream,
        copied: Stream,
    },
    ExpectedReserved {
        expected: &'static str,
        context: Construct,
    },
    UnexpectedAfterCommand {
        other: TokenKind,
    },
    BackgroundFunction {
        name: String,
    },
    UnmatchedEsac,
    Select,
    FunctionKeyword,
    UnexpectedReserved {
        other: &'static str,
    },
    InvalidFunctionName {
        name: String,
    },
    RejectedFunction {
        name: String,
        rejected: crate::builtins::RejectedCommand,
    },
    ReservedFunction {
        name: String,
    },
    ExpectedFunctionOpen {
        name: String,
    },
    ExpectedFunctionClose {
        name: String,
    },
    CStyleFor,
    ExpectedForVariable,
    ForVariableNotPlain,
    InvalidForVariable {
        variable: String,
    },
    ExpectedCaseSubject,
    ExpectedCasePatternClose,
    ExpectedCasePattern {
        found: Found,
    },
    CasePattern {
        character: char,
    },
    ExpectedCondition {
        found: Found,
    },
    TooManyConditionOperands,
    RegexCondition,
    ConditionalPattern {
        character: char,
    },
    ExpectedRedirectBuffer,
    SelfDuplication {
        source: Stream,
        target: Stream,
    },
    ArrayLiteral,
    ArithmeticCommand,
    Subshell,
    MultipleHereDocuments,
    ProcessSubstitution,
    InputRedirect,
    ExpectedCommand {
        found: Found,
    },
    ParameterPattern {
        character: char,
    },
    ArithmeticTokenLimit,
    InvalidArithmeticLiteral {
        literal: String,
    },
    ArithmeticLiteralOutOfRange {
        literal: String,
    },
    ExpectedArithmeticVariable,
    ArithmeticCharacter {
        character: char,
    },
    EmptyArithmetic,
    TrailingArithmeticTokens,
    UnbalancedArithmetic,
    UnexpectedArithmeticSymbol {
        symbol: &'static str,
    },
    ArithmeticEnded,
    ExpectedGroupClose,
    EmptyGroup,
    ExpectedConditionalClose {
        found: Found,
    },
    ExpectedCaseClauseEnd,
    ExpectedTestGroupClose,
    ArithmeticPower,
    ArithmeticIncrement,
    ArithmeticDecrement,
    ArithmeticCompoundAssignment,
    ArithmeticBitwise,
    ArithmeticTernary,
    ArithmeticAssignment,
    ArithmeticComma,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedToken { kind } => write!(formatter, "unexpected {kind}"),
            Self::UnterminatedSubstitution => formatter.write_str("unterminated $( ... )"),
            Self::UnterminatedHereDocument => formatter.write_str("unterminated here-document"),
            Self::DuplicationOrder { source, copied } => write!(formatter, "`{}>&{}` must come after the redirection of stream {}, not before it: write `> name {}>&{}` rather than `{}>&{} > name`", source.descriptor(), copied.descriptor(), copied.descriptor(), source.descriptor(), copied.descriptor(), source.descriptor(), copied.descriptor()),
            Self::ExpectedReserved { expected, context } => write!(formatter, "expected `{expected}` in {}", context.name()),
            Self::UnexpectedAfterCommand { other } => write!(formatter, "unexpected {other} after a command"),
            Self::BackgroundFunction { name } => write!(formatter, "`&` cannot follow the function definition `{}() {{ ... }}`: define the function, then start it as a job with `{} &`", name, name),
            Self::UnmatchedEsac => formatter.write_str("`esac` without a matching `case`"),
            Self::Select => formatter.write_str("`select` is not part of this shell: there is no interactive terminal to prompt"),
            Self::FunctionKeyword => formatter.write_str("the `function` keyword is not part of this shell; define functions as `name() { ... }`"),
            Self::UnexpectedReserved { other } => write!(formatter, "unexpected `{other}`"),
            Self::InvalidFunctionName { name } => write!(formatter, "{name:?} is not a valid function name"),
            Self::RejectedFunction { name, rejected } => write!(formatter, "cannot define a function named {name:?}: {}", rejected.reason()),
            Self::ReservedFunction { name } => write!(formatter, "cannot define a function named {name:?}: it is a reserved word"),
            Self::ExpectedFunctionOpen { name } => write!(formatter, "expected `{{` to open the body of function {name:?}"),
            Self::ExpectedFunctionClose { name } => write!(formatter, "expected `}}` to close the body of function {name:?}"),
            Self::CStyleFor => formatter.write_str("C-style `for (( ... ))` loops are not supported; use `for x in ...` over a list, or a `while` loop with `i=$(( i + 1 ))`"),
            Self::ExpectedForVariable => formatter.write_str("expected a `for` loop variable"),
            Self::ForVariableNotPlain => formatter.write_str("a `for` loop variable must be a plain name"),
            Self::InvalidForVariable { variable } => write!(formatter, "{variable:?} is not a valid `for` loop variable name"),
            Self::ExpectedCaseSubject => formatter.write_str("expected a word to match after `case`"),
            Self::ExpectedCasePatternClose => formatter.write_str("expected `)` to close a `case` pattern list"),
            Self::ExpectedCasePattern { found } => write!(formatter, "expected a `case` pattern, found {found}"),
            Self::CasePattern { character } => write!(formatter,
        "`case` patterns are literal: `{character}` is not supported; quote it as `'{character}'`, list alternatives with `|`, or use `*)` for the default"
    ),
            Self::ExpectedCondition { found } => write!(formatter, "expected a condition inside `[[ ... ]]`, found {found}"),
            Self::TooManyConditionOperands => formatter.write_str("a `[[ ... ]]` condition takes at most three operands; join conditions with `&&`,                  `||`, or parentheses"),
            Self::RegexCondition => formatter.write_str("`=~` regex matching is not supported: every pattern in this shell is literal text; compare with `==`, or match structurally with `jq`"),
            Self::ConditionalPattern { character } => write!(formatter,
        "`[[ == ]]` patterns are literal: `{character}` is not supported; quote it as `'{character}'` or use `jq`"
    ),
            Self::ExpectedRedirectBuffer => formatter.write_str("expected an in-memory buffer name after a redirection operator"),
            Self::SelfDuplication { source, target } => write!(formatter, "`{}>&{}` redirects a stream onto itself and would do nothing", source.descriptor(), target.descriptor()),
            Self::ArrayLiteral => formatter.write_str("bash array literals `name=(a b c)` are not supported: arrays here are real JSON, so write `name='[\"a\",\"b\",\"c\"]'` or `name=$(... | jq ...)` and index it with `${name[0]}`"),
            Self::ArithmeticCommand => formatter.write_str("the arithmetic command `(( ... ))` is not supported; use the arithmetic expansion `x=$(( ... ))`, or `[ ... ]` to test a value"),
            Self::Subshell => formatter.write_str("subshells `( ... )` are not supported: this shell forks no processes; use a function instead"),
            Self::MultipleHereDocuments => formatter.write_str("a command accepts at most one here-document"),
            Self::ProcessSubstitution => formatter.write_str("process substitution `<( ... )` is not supported: this shell forks no processes and has no file descriptors"),
            Self::InputRedirect => formatter.write_str("input redirection `<` is not supported: there are no files; pipe a value or `cat` a named buffer instead"),
            Self::ExpectedCommand { found } => write!(formatter, "expected a command, found {found}"),
            Self::ParameterPattern { character } => write!(formatter, "`${{NAME}}` patterns are literal: `{character}` is not supported; slice with `jq`"),
            Self::ArithmeticTokenLimit => write!(formatter,
                    "an arithmetic expansion may hold at most {MAX_ARITHMETIC_TOKENS} tokens; split the calculation across assignments"
                ),
            Self::InvalidArithmeticLiteral { literal } => write!(formatter, "invalid arithmetic literal {literal:?}"),
            Self::ArithmeticLiteralOutOfRange { literal } => write!(formatter, "arithmetic literal {literal:?} is out of range"),
            Self::ExpectedArithmeticVariable => formatter.write_str("expected a variable name after `$` in an arithmetic expansion"),
            Self::ArithmeticCharacter { character } => write!(formatter, "unsupported character {character:?} in an arithmetic expansion"),
            Self::EmptyArithmetic => formatter.write_str("empty arithmetic expansion"),
            Self::TrailingArithmeticTokens => formatter.write_str("trailing tokens in an arithmetic expansion"),
            Self::UnbalancedArithmetic => formatter.write_str("unbalanced parentheses in an arithmetic expansion"),
            Self::UnexpectedArithmeticSymbol { symbol } => write!(formatter, "unexpected `{symbol}` in an arithmetic expansion"),
            Self::ArithmeticEnded => formatter.write_str("arithmetic expansion ended unexpectedly"),
            Self::ExpectedGroupClose => formatter.write_str("expected `}` to close a `{ ...; }` group"),
            Self::EmptyGroup => formatter.write_str("an empty `{ }` group runs nothing; remove it or put a command in it"),
            Self::ExpectedConditionalClose { found } => write!(formatter, "expected `]]` to close a `[[ ... ]]` conditional, found {found}"),
            Self::ExpectedCaseClauseEnd => formatter.write_str("expected `;;` to end a `case` clause"),
            Self::ExpectedTestGroupClose => formatter.write_str("expected `)` to close a group inside `[[ ... ]]`"),
            Self::ArithmeticPower => formatter.write_str("`**` is not supported; multiply repeatedly, or use `jq pow`"),
            Self::ArithmeticIncrement => formatter.write_str("`++` is not supported; write `i=$(( i + 1 ))`"),
            Self::ArithmeticDecrement => formatter.write_str("`--` is not supported; write `i=$(( i - 1 ))`"),
            Self::ArithmeticCompoundAssignment => formatter.write_str("compound assignment is not supported inside `$(( ... ))`; write `name=$(( name + 1 ))`"),
            Self::ArithmeticBitwise => formatter.write_str("bitwise operators are not supported; this arithmetic is numeric only, so use `jq` for bit manipulation"),
            Self::ArithmeticTernary => formatter.write_str("the ternary `? :` is not supported; use `if`/`else`"),
            Self::ArithmeticAssignment => formatter.write_str("assignment inside `$(( ... ))` is not supported; assign the expansion instead, as `name=$(( ... ))`"),
            Self::ArithmeticComma => formatter.write_str("the comma operator is not supported; write one expansion per value"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TooDeep {
    Grammar { construct: Construct },
    CommandSubstitution,
    Parameter,
    ArithmeticOperators,
}

impl std::fmt::Display for TooDeep {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Grammar { construct } => write!(
                formatter,
                "{} nested more than {MAX_NESTING_DEPTH} levels deep; this shell parses on a fixed stack and refuses rather than risking it",
                construct.name()
            ),
            Self::CommandSubstitution => write!(
                formatter,
                "command substitution `$( ... )` nested more than {MAX_NESTING_DEPTH} levels deep; this shell parses on a fixed stack and refuses rather than risking it"
            ),
            Self::Parameter => write!(
                formatter,
                "parameter expansions nested deeper than {}",
                super::depth::MAX_PARAMETER_NESTING
            ),
            Self::ArithmeticOperators => write!(
                formatter,
                "an arithmetic expansion may nest at most {MAX_NESTING_DEPTH} operators deep, counting each `!`, unary `-` and operator in a chain like `a + b + c`; split the calculation across assignments"
            ),
        }
    }
}
