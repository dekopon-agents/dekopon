//! Constructs deliberately dropped, such as backgrounding, subshells, process substitution, and
//! eval, have no representation here, so no evaluator path can accidentally implement one.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Program {
    pub statements: Vec<Statement>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Statement {
    List(AndOrList),
    If(IfStatement),
    For(ForLoop),
    While(WhileLoop),
    Case(CaseStatement),
    Group(Program),
    Conditional(Conditional),
    Function(FunctionDefinition),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AndOrList {
    pub first: Pipeline,
    pub rest: Vec<(AndOr, Pipeline)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AndOr {
    And,
    Or,
}

/// A pipe hands the single structured value produced by the left command to the right as its
/// implicit input; this is jq-style value piping, not byte-stream piping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
    pub negated: bool,
}

/// A compound stage (if/for/while/case/group) runs in the current scope, not a subshell, so a
/// variable a piped while loop assigns is still set afterward, the opposite of bash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Simple(SimpleCommand),
    Compound {
        statement: Box<Statement>,
        redirects: Vec<Redirect>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SimpleCommand {
    pub assignments: Vec<Assignment>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
    pub here_doc: Option<Word>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Assignment {
    pub name: String,
    pub value: Word,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stream {
    Stdout,
    Stderr,
    Both,
}

impl Stream {
    #[must_use]
    pub const fn descriptor(self) -> &'static str {
        match self {
            Self::Stdout => "1",
            Self::Stderr => "2",
            Self::Both => "&",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedirectTarget {
    /// Not files: the buffer store lives for exactly one script execution and is unreachable from
    /// any real path; the reserved name DEV_NULL discards.
    Buffer {
        append: bool,
        target: Word,
    },
    Stream(Stream),
}

pub const DEV_NULL: &str = "/dev/null";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redirect {
    pub source: Stream,
    pub target: RedirectTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IfStatement {
    pub branches: Vec<(AndOrList, Program)>,
    pub otherwise: Option<Program>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForLoop {
    pub variable: String,
    pub words: Vec<Word>,
    pub body: Program,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WhileLoop {
    pub condition: AndOrList,
    pub body: Program,
    pub until: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaseStatement {
    pub subject: Word,
    pub clauses: Vec<CaseClause>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaseClause {
    pub patterns: Vec<CasePattern>,
    pub body: Program,
}

/// Unlike bash's filename-style globs, this matches literal text, since a partial wildcard would
/// otherwise mismatch silently; a bare * is kept only as the default branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CasePattern {
    Any,
    Literal(Word),
    Expanded(Word),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionDefinition {
    pub name: String,
    pub body: Program,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Word {
    pub parts: Vec<WordPart>,
}

impl Word {
    #[must_use]
    #[cfg(test)]
    pub(crate) fn as_literal(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [WordPart::Literal(text)] => Some(text),
            _ => None,
        }
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn is_bare_command_substitution(&self) -> bool {
        matches!(self.parts.as_slice(), [WordPart::CommandSubstitution(_)])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WordPart {
    /// Unquoted literal text. `*`, `?`, `[`, `{`, and `~` are ordinary characters here.
    Literal(String),
    SingleQuoted(String),
    DoubleQuoted(Vec<WordPart>),
    Parameter(Parameter),
    CommandSubstitution(Program),
    Arithmetic(ArithExpr),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Parameter {
    Named {
        name: String,
        indices: Vec<Index>,
        modifier: Modifier,
        length: bool,
    },
    Positional(usize),
    AllPositional,
    AllPositionalJoined,
    PositionalCount,
    LastStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Index {
    At(Word),
    All,
    AllJoined,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Modifier {
    None,
    Default {
        colon: bool,
        word: Word,
    },
    Assign {
        colon: bool,
        word: Word,
    },
    Require {
        colon: bool,
        word: Option<Word>,
    },
    Alternate {
        colon: bool,
        word: Word,
    },
    StripPrefix(Pattern),
    StripSuffix(Pattern),
    Replace {
        all: bool,
        pattern: Pattern,
        replacement: Word,
    },
}

/// Operand tests share code with test and [, so they cannot disagree on what -z or -lt means;
/// unlike [, an unquoted expansion here is always one word even with embedded spaces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Conditional {
    Test(ConditionalTest),
    Not(Box<Conditional>),
    And(Box<Conditional>, Box<Conditional>),
    Or(Box<Conditional>, Box<Conditional>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionalTest {
    pub words: Vec<Word>,
    /// This operand is a bash glob; a metacharacter is rejected by name at parse time when
    /// constant, or at expansion otherwise, and this flag tracks which applies.
    pub check_right_pattern: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Pattern {
    Literal(Word),
    Expanded(Word),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArithExpr {
    Integer(i64),
    Float(f64),
    Variable(String),
    Unary(ArithUnaryOp, Box<ArithExpr>),
    Binary(ArithBinaryOp, Box<ArithExpr>, Box<ArithExpr>),
}

impl Eq for ArithExpr {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithUnaryOp {
    Negate,
    Not,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithBinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Equal,
    NotEqual,
    And,
    Or,
}
