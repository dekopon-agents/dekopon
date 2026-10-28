use super::{MAX_ARITHMETIC_TOKENS, MAX_NESTING_DEPTH};
use crate::{ast::Stream, builtins::RejectedCommand, lexer::TokenKind};
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ParseError {
    Refused {
        line: usize,
        refusal: Refusal,
    },
    TooDeep {
        line: usize,
        construct: DepthConstruct,
    },
}
impl ParseError {
    pub(crate) fn refused(line: usize, refusal: Refusal) -> Self {
        Self::Refused { line, refusal }
    }
    #[cfg(test)]
    pub(crate) fn line(&self) -> usize {
        match self {
            Self::Refused { line, .. } | Self::TooDeep { line, .. } => *line,
        }
    }
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { line, refusal } => write!(f, "line {line}: {refusal}"),
            Self::TooDeep { line, construct } => write!(f, "line {line}: {construct}"),
        }
    }
}
impl std::error::Error for ParseError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Found {
    End,
    Token(Box<TokenKind>),
}
impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::End => f.write_str("end of script"),
            Self::Token(token) => token.fmt(f),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PatternContext {
    Case,
    Parameter,
    Conditional,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Keyword {
    If,
    Then,
    Fi,
    While,
    Until,
    For,
    In,
    Do,
    Done,
    Case,
    Esac,
}
impl Keyword {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::If => "if",
            Self::Then => "then",
            Self::Fi => "fi",
            Self::While => "while",
            Self::Until => "until",
            Self::For => "for",
            Self::In => "in",
            Self::Do => "do",
            Self::Done => "done",
            Self::Case => "case",
            Self::Esac => "esac",
        }
    }
}
impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Context {
    If,
    Elif,
    While,
    Until,
    For,
    Case,
}
impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::If => "an `if` statement",
            Self::Elif => "an `elif` branch",
            Self::While => "a `while` loop",
            Self::Until => "an `until` loop",
            Self::For => "a `for` loop",
            Self::Case => "a `case` statement",
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DepthConstruct {
    Block(super::nested::Construct),
    Substitution,
    Parameter,
    ArithmeticGroup,
    ArithmeticOperators,
}
impl fmt::Display for DepthConstruct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let construct = match self {
            Self::Block(construct) => construct.name(),
            Self::Substitution => "command substitution `$( ... )`",
            Self::ArithmeticGroup => "an arithmetic expansion",
            Self::Parameter => {
                return write!(
                    f,
                    "parameter expansions nested deeper than {}",
                    super::depth::MAX_PARAMETER_NESTING
                );
            }
            Self::ArithmeticOperators => {
                return write!(
                    f,
                    "an arithmetic expansion may nest at most {MAX_NESTING_DEPTH} operators deep, counting each `!`, unary `-` and operator in a chain like `a + b + c`; split the calculation across assignments"
                );
            }
        };
        write!(
            f,
            "{construct} nested more than {MAX_NESTING_DEPTH} levels deep; this shell parses on a fixed stack and refuses rather than risking it"
        )
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArithmeticOperator {
    Power,
    Increment,
    Decrement,
    CompoundAssignment,
    Bitwise,
    Assignment,
    Ternary,
    Comma,
}
impl fmt::Display for ArithmeticOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
Self::Power => f.write_str("`**` is not supported; multiply repeatedly, or use `jq pow`"),
Self::Increment => f.write_str("`++` is not supported; write `i=$(( i + 1 ))`"),
Self::Decrement => f.write_str("`--` is not supported; write `i=$(( i - 1 ))`"),
Self::CompoundAssignment => f.write_str("compound assignment is not supported inside `$(( ... ))`; write `name=$(( name + 1 ))`"),
Self::Bitwise => f.write_str("bitwise operators are not supported; this arithmetic is numeric only, so use `jq` for bit manipulation"),
Self::Assignment => f.write_str("assignment inside `$(( ... ))` is not supported; assign the expansion instead, as `name=$(( ... ))`"),
Self::Ternary => f.write_str("the ternary `? :` is not supported; use `if`/`else`"),
Self::Comma => f.write_str("the comma operator is not supported; write one expansion per value"),
}
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    UnterminatedSubstitution,
    UnterminatedHereDocument,
    UnmatchedEsac,
    Select,
    FunctionKeyword,
    CStyleFor,
    ExpectedForVariable,
    NonLiteralForVariable,
    ExpectedCaseSubject,
    ExpectedCasePatternClose,
    TooManyConditionOperands,
    RegexCondition,
    ExpectedBuffer,
    ArrayLiteral,
    ArithmeticCommand,
    Subshell,
    UnexpectedRightParen,
    MultipleHereDocuments,
    ProcessSubstitution,
    InputRedirection,
    ExpectedArithmeticVariable,
    EmptyArithmetic,
    TrailingArithmetic,
    UnbalancedArithmetic,
    ArithmeticEnded,
    ExpectedGroupClose,
    EmptyGroup,
    ExpectedCaseClauseEnd,
    ExpectedTestGroupClose,
    TrailingBackslash,
    UnterminatedSingleQuote,
    UnterminatedQuotedDelimiter,
    SplitHereDelimiter,
    ExpectedHereDelimiter,
    EmptyParameter,
    UnterminatedParameter,
    UnterminatedIndex,
    EmptyIndex,
    MultipleIndexExpressions,
    UnexpectedToken(Box<TokenKind>),
    DuplicationOrder {
        source: Stream,
        copied: Stream,
    },
    ExpectedKeyword {
        expected: Keyword,
        context: Context,
    },
    UnexpectedAfterCommand(Box<TokenKind>),
    BackgroundFunction(String),
    UnexpectedKeyword(String),
    InvalidFunctionName(String),
    RejectedFunction(RejectedCommand),
    ReservedFunction(String),
    ExpectedFunctionOpen(String),
    ExpectedFunctionClose(String),
    SelfDuplication {
        source: Stream,
        target: Stream,
    },
    InvalidForVariable(String),
    ExpectedCasePattern(Found),
    ExpectedCondition(Found),
    ExpectedCommand(Found),
    ExpectedConditionalClose(Found),
    Pattern {
        context: PatternContext,
        character: char,
    },
    ArithmeticTokens,
    InvalidArithmeticLiteral(String),
    ArithmeticLiteralOutOfRange(String),
    ArithmeticCharacter(char),
    UnexpectedArithmeticSymbol(&'static str),
    Backtick,
    UnknownDescriptor,
    InputDuplication,
    BothDuplication,
    DuplicationTarget,
    HereString,
    CaseFallthrough,
    MissingHereDelimiter(String),
    PositionalOutOfRange(String),
    IndexAfterAll(String),
    LengthWithTransformation {
        name: String,
        operator: char,
    },
    ColonInPattern {
        name: String,
        operator: char,
    },
    UnsupportedParameter {
        name: String,
        colon: bool,
        operator: char,
    },
    ParameterOperator {
        character: char,
        operator: Option<Box<TokenKind>>,
    },
    UnterminatedDoubleQuote,
    UnterminatedArithmetic,
    ArithmeticClosingText,
    ArithmeticOperator(ArithmeticOperator),
}
impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
Self::UnterminatedSubstitution => f.write_str("unterminated $( ... )"),
Self::UnterminatedHereDocument => f.write_str("unterminated here-document"),
Self::UnmatchedEsac => f.write_str("`esac` without a matching `case`"),
Self::Select => f.write_str("`select` is not part of this shell: there is no interactive terminal to prompt"),
Self::FunctionKeyword => f.write_str("the `function` keyword is not part of this shell; define functions as `name() { ... }`"),
Self::CStyleFor => f.write_str("C-style `for (( ... ))` loops are not supported; use `for x in ...` over a list, or a `while` loop with `i=$(( i + 1 ))`"),
Self::ExpectedForVariable => f.write_str("expected a `for` loop variable"),
Self::NonLiteralForVariable => f.write_str("a `for` loop variable must be a plain name"),
Self::ExpectedCaseSubject => f.write_str("expected a word to match after `case`"),
Self::ExpectedCasePatternClose => f.write_str("expected `)` to close a `case` pattern list"),
Self::TooManyConditionOperands => f.write_str("a `[[ ... ]]` condition takes at most three operands; join conditions with `&&`,                  `||`, or parentheses"),
Self::RegexCondition => f.write_str("`=~` regex matching is not supported: every pattern in this shell is literal text; compare with `==`, or match structurally with `jq`"),
Self::ExpectedBuffer => f.write_str("expected an in-memory buffer name after a redirection operator"),
Self::ArrayLiteral => f.write_str("bash array literals `name=(a b c)` are not supported: arrays here are real JSON, so write `name='[\"a\",\"b\",\"c\"]'` or `name=$(... | jq ...)` and index it with `${name[0]}`"),
Self::ArithmeticCommand => f.write_str("the arithmetic command `(( ... ))` is not supported; use the arithmetic expansion `x=$(( ... ))`, or `[ ... ]` to test a value"),
Self::Subshell => f.write_str("subshells `( ... )` are not supported: this shell forks no processes; use a function instead"),
Self::UnexpectedRightParen => f.write_str("unexpected `)`"),
Self::MultipleHereDocuments => f.write_str("a command accepts at most one here-document"),
Self::ProcessSubstitution => f.write_str("process substitution `<( ... )` is not supported: this shell forks no processes and has no file descriptors"),
Self::InputRedirection => f.write_str("input redirection `<` is not supported: there are no files; pipe a value or `cat` a named buffer instead"),
Self::ExpectedArithmeticVariable => f.write_str("expected a variable name after `$` in an arithmetic expansion"),
Self::EmptyArithmetic => f.write_str("empty arithmetic expansion"),
Self::TrailingArithmetic => f.write_str("trailing tokens in an arithmetic expansion"),
Self::UnbalancedArithmetic => f.write_str("unbalanced parentheses in an arithmetic expansion"),
Self::ArithmeticEnded => f.write_str("arithmetic expansion ended unexpectedly"),
Self::ExpectedGroupClose => f.write_str("expected `}` to close a `{ ...; }` group"),
Self::EmptyGroup => f.write_str("an empty `{ }` group runs nothing; remove it or put a command in it"),
Self::ExpectedCaseClauseEnd => f.write_str("expected `;;` to end a `case` clause"),
Self::ExpectedTestGroupClose => f.write_str("expected `)` to close a group inside `[[ ... ]]`"),
Self::TrailingBackslash => f.write_str("script ends with a trailing backslash"),
Self::UnterminatedSingleQuote => f.write_str("unterminated single-quoted string"),
Self::UnterminatedQuotedDelimiter => f.write_str("unterminated quoted here-document delimiter"),
Self::SplitHereDelimiter => f.write_str("a here-document delimiter cannot be split across lines; write it on the same line as `<<`"),
Self::ExpectedHereDelimiter => f.write_str("expected a here-document delimiter after `<<`, as in `cat <<EOF`"),
Self::EmptyParameter => f.write_str("empty ${} parameter reference"),
Self::UnterminatedParameter => f.write_str("unterminated ${} parameter reference"),
Self::UnterminatedIndex => f.write_str("unterminated ${NAME[index]} reference"),
Self::EmptyIndex => f.write_str("empty ${NAME[index]} reference"),
Self::MultipleIndexExpressions => f.write_str("${NAME[index]} accepts exactly one index expression"),
Self::UnexpectedToken(kind) => write!(f,"unexpected {kind}"),
Self::DuplicationOrder { source, copied } => write!(f,
                    "`{}>&{}` must come after the redirection of stream {}, not before it: \
                     write `> name {}>&{}` rather than `{}>&{} > name`",
                    source.descriptor(),
                    copied.descriptor(),
                    copied.descriptor(),
                    source.descriptor(),
                    copied.descriptor(),
                    source.descriptor(),
                    copied.descriptor(),
                ),
Self::ExpectedKeyword { expected, context } => write!(f,"expected `{expected}` in {context}"),
Self::UnexpectedAfterCommand(other) => write!(f,"unexpected {other} after a command"),
Self::BackgroundFunction(name) => write!(f,
                    "`&` cannot follow the function definition `{}() {{ ... }}`: define the function, then start it as a job with `{} &`",
                    name, name
                ),
Self::UnexpectedKeyword(other) => write!(f,"unexpected `{other}`"),
Self::InvalidFunctionName(name) => write!(f,"{name:?} is not a valid function name"),
Self::RejectedFunction(command) => write!(f, "cannot define a function named {:?}: {}", command.name(), command.reason()),
Self::ReservedFunction(name) => write!(f,"cannot define a function named {name:?}: it is a reserved word"),
Self::ExpectedFunctionOpen(name) => write!(f,"expected `{{` to open the body of function {name:?}"),
Self::ExpectedFunctionClose(name) => write!(f,"expected `}}` to close the body of function {name:?}"),
Self::SelfDuplication { source, target } => write!(f,
                                "`{}>&{}` redirects a stream onto itself and would do nothing",
                                source.descriptor(),
                                target.descriptor()
                            ),
Self::InvalidForVariable(variable) => write!(f,"{variable:?} is not a valid `for` loop variable name"),
Self::ExpectedCasePattern(found) => write!(f,"expected a `case` pattern, found {found}"),
Self::ExpectedCondition(found) => write!(f,"expected a condition inside `[[ ... ]]`, found {found}"),
Self::ExpectedCommand(found) => write!(f,"expected a command, found {found}"),
Self::ExpectedConditionalClose(found) => write!(f,"expected `]]` to close a `[[ ... ]]` conditional, found {found}"),
Self::Pattern { context: PatternContext::Case, character } => write!(f,
        "`case` patterns are literal: `{character}` is not supported; quote it as `'{character}'`, list alternatives with `|`, or use `*)` for the default"
    ),
Self::Pattern { context: PatternContext::Conditional, character } => write!(f,
        "`[[ == ]]` patterns are literal: `{character}` is not supported; quote it as `'{character}'` or use `jq`"
    ),
Self::Pattern { context: PatternContext::Parameter, character } => write!(f,"`${{NAME}}` patterns are literal: `{character}` is not supported; slice with `jq`"),
Self::ArithmeticTokens => write!(f,
                    "an arithmetic expansion may hold at most {MAX_ARITHMETIC_TOKENS} tokens; split the calculation across assignments"
                ),
Self::InvalidArithmeticLiteral(literal) => write!(f,"invalid arithmetic literal {literal:?}"),
Self::ArithmeticLiteralOutOfRange(literal) => write!(f,"arithmetic literal {literal:?} is out of range"),
Self::ArithmeticCharacter(character) => write!(f,"unsupported character {character:?} in an arithmetic expansion"),
Self::UnexpectedArithmeticSymbol(symbol) => write!(f,"unexpected `{symbol}` in an arithmetic expansion"),
Self::Backtick => f.write_str("backtick command substitution is not supported; use `$( ... )`, which nests and quotes cleanly"),
Self::UnknownDescriptor => f.write_str("only descriptors 1 (the value stream) and 2 (the diagnostic stream) exist in this shell; there is nothing else to redirect"),
Self::InputDuplication => f.write_str("input duplication (`<&`) is not supported: there is no input descriptor to duplicate; pipe a value or `cat` a named buffer instead"),
Self::BothDuplication => f.write_str("`&>&` is not a redirection: `&>` already sends both streams to one buffer, so there is no second stream left to duplicate"),
Self::DuplicationTarget => f.write_str("a duplication must name a stream: write `>&1` or `>&2`; to write a buffer use `> name`, `2> name`, or `&> name`"),
Self::HereString => f.write_str("the here-string `<<<` is not supported; pipe the value instead, as in `echo \"$x\" | cmd`, or use a here-document `<<EOF ... EOF`"),
Self::CaseFallthrough => f.write_str("`;&` and `;;&` are not supported: a `case` clause here runs alone and never falls through to the next; end every clause with `;;`"),
Self::MissingHereDelimiter(delimiter) => write!(f, "unterminated here-document: the script ended before a line containing exactly {delimiter:?}"),
Self::PositionalOutOfRange(digits) => write!(f,"positional parameter ${digits} is out of range"),
Self::IndexAfterAll(name) => write!(f,
                        "${{{name}[@]}} selects every element, so it cannot be indexed further"
                    ),
Self::LengthWithTransformation { name, operator: first } => write!(f,
                    "${{#{name}{first}...}} combines a length with a transformation; ask for one or the other"
                ),
Self::ColonInPattern { name, operator } => write!(f,
                        "`${{{name}:{operator}...}}` is not a parameter expansion; drop the colon"
                    ),
Self::UnsupportedParameter { name, colon, operator: other } => write!(f,
                        "unsupported ${{{name}{}{other}...}} parameter expansion; this shell keeps \
                         ${{NAME}}, ${{NAME[index]}}, ${{#NAME}}, `:-`, `:=`, `:?`, `:+`, `#`, `%`, \
                         and `/`",
                        if *colon { ":" } else { "" }
                    ),
Self::ParameterOperator { character, operator } => write!(f,
                            "`{}` is not allowed inside a ${{}} parameter expansion",
                            operator.as_deref().map_or_else(|| character.to_string(), ToString::to_string)
                        ),
Self::UnterminatedDoubleQuote => f.write_str("unterminated double-quoted string"),
Self::UnterminatedArithmetic => f.write_str("unterminated $(( ... ))"),
Self::ArithmeticClosingText => f.write_str("unexpected text between the closing parentheses of $(( ... ))"),
Self::ArithmeticOperator(operator) => operator.fmt(f),
}
    }
}
