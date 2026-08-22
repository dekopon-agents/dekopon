//! Abstract syntax produced by [`crate::parser`] and walked by the evaluator.
//!
//! The shape covers exactly the grammar this sandbox keeps. Constructs that were deliberately
//! dropped (backgrounding, subshells, process substitution, `eval`, brace groups) have no
//! representation here at all, so no evaluator path can accidentally implement one.

/// A parsed script or block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Program {
    /// Statements in source order.
    pub statements: Vec<Statement>,
}

/// One top-level or block-level statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Statement {
    /// An `&&`/`||` list of pipelines.
    List(AndOrList),
    /// `if ...; then ...; elif ...; else ...; fi`.
    If(IfStatement),
    /// `for NAME in WORDS...; do ...; done`.
    For(ForLoop),
    /// `while LIST; do ...; done` or `until LIST; do ...; done`.
    While(WhileLoop),
    /// `case WORD in PATTERN) ...;; esac`.
    Case(CaseStatement),
    /// `name() { ... }`.
    Function(FunctionDefinition),
}

/// A pipeline chain joined by short-circuiting `&&` and `||`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AndOrList {
    /// The unconditional first pipeline.
    pub first: Pipeline,
    /// Conditionally executed continuations.
    pub rest: Vec<(AndOr, Pipeline)>,
}

/// Short-circuit operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AndOr {
    /// `&&`: run when the previous status was zero.
    And,
    /// `||`: run when the previous status was non-zero.
    Or,
}

/// One or more commands joined by `|`.
///
/// A pipe hands the single structured value produced by the left command to the right command as
/// its implicit input. This is jq-style value piping, not byte-stream piping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pipeline {
    /// Commands in left-to-right order; never empty.
    pub commands: Vec<SimpleCommand>,
    /// `true` when a leading `!` inverts the pipeline's exit status.
    pub negated: bool,
}

/// One command: optional assignment prefixes, argv words, and its redirections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SimpleCommand {
    /// `NAME=value` prefixes. With no argv words these are plain assignments.
    pub assignments: Vec<Assignment>,
    /// Command word followed by arguments.
    pub words: Vec<Word>,
    /// Redirections in source order, applied left to right the way bash applies them.
    pub redirects: Vec<Redirect>,
    /// `<<DELIM` body, supplying this command's input in place of anything piped into it.
    pub here_doc: Option<Word>,
}

/// A `NAME=value` assignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Assignment {
    /// Variable name.
    pub name: String,
    /// Right-hand side.
    pub value: Word,
}

/// One of the two streams a command writes to.
///
/// A command produces a *value* on stdout and *text* on stderr. That split already governed how
/// this interpreter behaved — a command substitution captures the value and lets diagnostics
/// through to the terminal, exactly as a real shell does — and these are the names a script uses to
/// address the two halves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stream {
    /// The value channel, written by `>`, `1>`, and read by `$( )`.
    Stdout,
    /// The diagnostic channel, written by `2>`.
    Stderr,
    /// Both at once, written by `&>`. Never a duplication *target*.
    Both,
}

impl Stream {
    /// Renders the descriptor prefix a script would have typed.
    #[must_use]
    pub const fn descriptor(self) -> &'static str {
        match self {
            Self::Stdout => "1",
            Self::Stderr => "2",
            Self::Both => "&",
        }
    }
}

/// Where a redirected stream goes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RedirectTarget {
    /// A named in-memory buffer.
    ///
    /// These are not files. The buffer store lives for exactly one script execution and is
    /// unreachable from any real path; `cat <name>` is the only reader. The one reserved name is
    /// [`DEV_NULL`], which discards.
    Buffer {
        /// `true` for `>>` and `2>>`, `false` for `>` and `2>`.
        append: bool,
        /// Buffer name word.
        target: Word,
    },
    /// The other stream, as in `2>&1` and `>&2`. Never [`Stream::Both`].
    Stream(Stream),
}

/// The one buffer name that discards everything written to it.
///
/// There is no filesystem here, so this is a reserved name rather than a path. It exists because it
/// is the one target a model will reach for to silence a command, and refusing the spelling every
/// shell shares would be worse than admitting it.
pub const DEV_NULL: &str = "/dev/null";

/// One redirection: which stream, and where it goes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redirect {
    /// The stream being redirected.
    pub source: Stream,
    /// Its destination.
    pub target: RedirectTarget,
}

/// `if`/`elif`/`else`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IfStatement {
    /// `if` and each `elif` condition paired with its body.
    pub branches: Vec<(AndOrList, Program)>,
    /// Optional `else` body.
    pub otherwise: Option<Program>,
}

/// `for NAME in WORDS...; do ...; done`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForLoop {
    /// Loop variable.
    pub variable: String,
    /// Words expanded once before the loop begins.
    pub words: Vec<Word>,
    /// Loop body.
    pub body: Program,
}

/// `while LIST; do ...; done` and its inverted `until` form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WhileLoop {
    /// Condition re-evaluated before each iteration.
    pub condition: AndOrList,
    /// Loop body.
    pub body: Program,
    /// `true` for `until`, which iterates while the condition's status is non-zero.
    pub until: bool,
}

/// `case WORD in PATTERN) ...;; esac`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaseStatement {
    /// The word each clause's patterns are matched against.
    pub subject: Word,
    /// Clauses in source order; the first whose pattern matches runs, and no other.
    pub clauses: Vec<CaseClause>,
}

/// One `PATTERN|PATTERN) LIST ;;` clause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaseClause {
    /// Alternatives, any one of which selects this clause.
    pub patterns: Vec<CasePattern>,
    /// Body run when a pattern matches.
    pub body: Program,
}

/// One `case` alternative.
///
/// Bash matches these as filename-style patterns. This shell matches literal text instead, for the
/// same reason `builtins`' `grep` and `sed` take literal patterns: a partial wildcard is
/// the pattern a literal matcher answers wrongly and silently, so it is rejected by name rather
/// than quietly mismatched. A bare `*` is kept, because it is the default branch rather than a
/// wildcard in any meaningful sense.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CasePattern {
    /// A bare `*`: the catch-all branch, which matches every subject.
    Any,
    /// A constant pattern, already checked for pattern syntax when it was parsed.
    Literal(Word),
    /// A pattern built from expansions, checked for pattern syntax when it is expanded.
    ///
    /// It cannot be checked earlier, because its text does not exist until the script runs — the
    /// same reason `grep "$pattern"` is checked at run time rather than at parse time.
    Expanded(Word),
}

/// `name() { ... }`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FunctionDefinition {
    /// Function name.
    pub name: String,
    /// Function body.
    pub body: Program,
}

/// One argv word built from concatenated parts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Word {
    /// Parts in source order.
    pub parts: Vec<WordPart>,
}

impl Word {
    /// Returns the word's text when it is a single unquoted literal.
    #[must_use]
    pub fn as_literal(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [WordPart::Literal(text)] => Some(text),
            _ => None,
        }
    }

    /// Reports whether the whole word is exactly one command substitution.
    ///
    /// `x=$(cmd)` preserves the command's structured value instead of coercing it to text. This is
    /// a deliberate, documented deviation from bash, where `$()` is always textual; it is what lets
    /// `ip=$(curl ...)` be followed by `echo ${ip[origin]}`.
    #[must_use]
    pub fn is_bare_command_substitution(&self) -> bool {
        matches!(self.parts.as_slice(), [WordPart::CommandSubstitution(_)])
    }
}

/// One component of a word.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WordPart {
    /// Unquoted literal text. `*`, `?`, `[`, `{`, and `~` are ordinary characters here.
    Literal(String),
    /// Single-quoted text; fully literal, bash-exact.
    SingleQuoted(String),
    /// Double-quoted text; interpolates parameters, `$(...)`, and `$(( ... ))`.
    DoubleQuoted(Vec<WordPart>),
    /// An unquoted parameter reference. A JSON array expands element-by-element into argv words.
    Parameter(Parameter),
    /// `$( ... )`.
    CommandSubstitution(Program),
    /// `$(( ... ))`.
    Arithmetic(ArithExpr),
}

/// A parameter reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Parameter {
    /// `$NAME`, `${NAME}`, `${NAME[index]}`.
    Named {
        /// Variable name.
        name: String,
        /// Index words applied left to right; array offsets and object keys are backed by real JSON.
        indices: Vec<Word>,
    },
    /// `$1` .. `${N}`.
    Positional(usize),
    /// `$@`, which splits one word per parameter even inside double quotes.
    AllPositional,
    /// `$*`, which is always exactly one space-joined word.
    AllPositionalJoined,
    /// `$#`.
    PositionalCount,
    /// `$?`.
    LastStatus,
}

/// An arithmetic expression inside `$(( ... ))`.
#[derive(Clone, Debug, PartialEq)]
pub enum ArithExpr {
    /// Integer literal.
    Integer(i64),
    /// Floating-point literal.
    Float(f64),
    /// A bare variable name; its value is coerced to a number.
    Variable(String),
    /// Prefix `-` or `!`.
    Unary(ArithUnaryOp, Box<ArithExpr>),
    /// An infix operator.
    Binary(ArithBinaryOp, Box<ArithExpr>, Box<ArithExpr>),
}

impl Eq for ArithExpr {}

/// Prefix arithmetic operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithUnaryOp {
    /// Arithmetic negation.
    Negate,
    /// Logical negation, yielding `1` or `0`.
    Not,
}

/// Infix arithmetic operator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithBinaryOp {
    /// `+`.
    Add,
    /// `-`.
    Subtract,
    /// `*`.
    Multiply,
    /// `/`.
    Divide,
    /// `%`.
    Remainder,
    /// `<`.
    Less,
    /// `<=`.
    LessOrEqual,
    /// `>`.
    Greater,
    /// `>=`.
    GreaterOrEqual,
    /// `==`.
    Equal,
    /// `!=`.
    NotEqual,
    /// `&&`.
    And,
    /// `||`.
    Or,
}
