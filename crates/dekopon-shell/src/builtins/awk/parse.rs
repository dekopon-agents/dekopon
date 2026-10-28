use std::{collections::BTreeMap, fmt};

use dekopon_process::LimitExceeded;

use super::lex::{Lexer, Punct, Token, TokenKind};
use crate::builtins::{CommandFailure, FatalError};

#[derive(Debug, PartialEq)]
pub(super) enum AwkError {
    Refused(Refusal),
    Syntax {
        line: usize,
        column: usize,
        expected: String,
    },
    Runtime {
        error: RuntimeError,
        record: f64,
    },
    Limit(LimitExceeded),
}

#[derive(Debug, PartialEq)]
pub(super) enum RuntimeError {
    DivisionByZero,
    FieldIndex(f64),
    FieldCount,
    InvalidUtf8,
    EmptySeparator,
    Regex(String),
    Format(Refusal),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum GawkVariable {
    IgnoreCase,
    Fpat,
    FieldWidths,
    ProcInfo,
    Rt,
    BinMode,
    Lint,
    TextDomain,
    Errno,
}
impl GawkVariable {
    pub(super) fn lookup(name: &str) -> Option<Self> {
        [
            Self::IgnoreCase,
            Self::Fpat,
            Self::FieldWidths,
            Self::ProcInfo,
            Self::Rt,
            Self::BinMode,
            Self::Lint,
            Self::TextDomain,
            Self::Errno,
        ]
        .into_iter()
        .find(|v| v.name() == name)
    }
    fn name(self) -> &'static str {
        match self {
            Self::IgnoreCase => "IGNORECASE",
            Self::Fpat => "FPAT",
            Self::FieldWidths => "FIELDWIDTHS",
            Self::ProcInfo => "PROCINFO",
            Self::Rt => "RT",
            Self::BinMode => "BINMODE",
            Self::Lint => "LINT",
            Self::TextDomain => "TEXTDOMAIN",
            Self::Errno => "ERRNO",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum GawkSyntax {
    BeginFile,
    EndFile,
    Include,
    Load,
    Switch,
}
impl GawkSyntax {
    pub(super) fn lookup(name: &str) -> Option<Self> {
        [
            Self::BeginFile,
            Self::EndFile,
            Self::Include,
            Self::Load,
            Self::Switch,
        ]
        .into_iter()
        .find(|v| v.name() == name)
    }
    fn name(self) -> &'static str {
        match self {
            Self::BeginFile => "BEGINFILE",
            Self::EndFile => "ENDFILE",
            Self::Include => "@include",
            Self::Load => "@load",
            Self::Switch => "switch",
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum Refusal {
    Getline,
    Process,
    OutputRedirection,
    StreamFunction(String),
    Randomness(String),
    UserFunction,
    UnknownFunction(String),
    RecordSeparator,
    NumberFormat(String),
    Environment(String),
    GawkVariable(GawkVariable),
    MultiSubscript,
    PowerSpelling,
    Nextfile,
    GawkSyntax(GawkSyntax),
    PrintComparison(String),
    NextOutsideRule,
    ScalarArray(String),
    Option(String),
    OperandAssignment(String),
    SecondInput,
    NoProgram,
    BadAssignment(String),
    SeparatorRegex(String),
    EmptySeparator,
    PrintfStar,
    PrintfConversion(char),
    ProgramBytes(usize),
    ParseDepth,
    AstHeight,
    PrintfWidth(usize),
    PrintfPrecision(usize),
    RegexExtension,
    Regex(String),
    InternalCaptures(usize),
    ParserStart(String),
    Internal(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Getline => f.write_str("awk: `getline` is not supported: awk reads only its one input, one record per rule pass; write a rule per record, or join inputs with the shell before the pipe"),
            Self::Process => f.write_str("awk: `system()` is not supported: this shell runs no processes; run the command in the script and pipe its output into awk"),
            Self::OutputRedirection => f.write_str("awk: output redirection is not supported: awk writes only to its stdout; redirect the whole command (`awk '…' > buf`, `awk '…' >&2`), or parenthesize a comparison: `print (a > b)`"),
            Self::StreamFunction(name) => write!(f, "awk: `{name}()` is not supported: awk opens no files or pipes here"),
            Self::Randomness(name) => write!(f, "awk: `{name}()` is not supported: the shell has no clock or entropy source"),
            Self::UserFunction => f.write_str("awk: user-defined functions are not supported: write the expression inline, or call awk from a shell function"),
            Self::UnknownFunction(name) => write!(f, "awk: `{name}()` is not a function here; the built-in functions are length substr index split sub gsub match sprintf tolower toupper int sqrt exp log sin cos atan2"),
            Self::RecordSeparator => f.write_str("awk: `RS` is not supported: records are always lines; split a record yourself with `split($0, parts, sep)`"),
            Self::NumberFormat(name) => write!(f, "awk: `{name}` is not supported: numbers print as integers or with %.6g; use printf or sprintf for another format"),
            Self::Environment(name) => write!(f, "awk: `{name}` is not supported: awk sees only its input and its -v assignments; pass values with `-v name=value`"),
            Self::GawkVariable(variable) => match variable {
                GawkVariable::IgnoreCase => f.write_str("awk: `IGNORECASE` is a gawk variable and is not supported; for case-insensitive matching compare `tolower($0) ~ /re/`"),
                name @ (GawkVariable::Fpat | GawkVariable::FieldWidths | GawkVariable::ProcInfo | GawkVariable::Rt | GawkVariable::BinMode | GawkVariable::Lint | GawkVariable::TextDomain | GawkVariable::Errno) => write!(f, "awk: `{}` is a gawk variable and is not supported; use FS, split() or substr()", name.name()),
            },
            Self::MultiSubscript => f.write_str("awk: multi-dimensional subscripts are not supported: join the keys yourself, `a[i \",\" j]`"),
            Self::PowerSpelling => f.write_str("awk: `**` is not supported: POSIX awk spells power `^`"),
            Self::Nextfile => f.write_str("awk: `nextfile` is not supported: awk has one input; use `exit` to stop reading"),
            Self::GawkSyntax(syntax) => match syntax {
                GawkSyntax::Switch => f.write_str("awk: `switch` is a gawk extension and is not supported; use if/else"),
                name @ (GawkSyntax::BeginFile | GawkSyntax::EndFile | GawkSyntax::Include | GawkSyntax::Load) => write!(f, "awk: `{}` is a gawk extension and is not supported; awk has one input", name.name()),
            },
            Self::PrintComparison(op) => write!(f, "awk: an unparenthesized comparison in a print list is not POSIX awk; write `print (a {op} b)`"),
            Self::NextOutsideRule => f.write_str("awk: `next` is only allowed in a rule, not in BEGIN or END"),
            Self::ScalarArray(name) => write!(f, "awk: `{name}` is used both as an array and as a scalar"),
            Self::Option(flag) => f.write_str(&super::super::unsupported_flag_text("awk", flag, "-F SEP -v NAME=VALUE")),
            Self::OperandAssignment(value) => write!(f, "awk: operand assignment `{value}` is not supported: use `-v {value}` before the program"),
            Self::SecondInput => f.write_str("awk: at most one input operand (a named buffer or `-`); join buffers with `cat a b | awk '…'`"),
            Self::NoProgram => f.write_str("awk: a program argument is required"),
            Self::BadAssignment(value) => write!(f, "awk: -v needs NAME=VALUE with NAME an identifier, got `{value}`"),
            Self::SeparatorRegex(message) | Self::Regex(message) => f.write_str(message),
            Self::EmptySeparator => f.write_str("awk: an empty field separator is not supported; take characters with `substr($0, i, 1)`"),
            Self::PrintfStar => f.write_str("awk: printf `*` width is not supported; build the format with sprintf or concatenation"),
            Self::PrintfConversion(conversion) => write!(f, "awk: printf conversion `%{conversion}` is not supported; use %c %d %i %o %u %x %X %e %E %f %F %g %G %s or %%"),
            Self::ProgramBytes(bytes) => write!(f, "awk: program is {bytes} bytes; the limit is 8192"),
            Self::ParseDepth => f.write_str("awk: program nests too deeply (more than 96 parser levels); split it across statements"),
            Self::AstHeight => f.write_str("awk: program nests deeper than 32 levels; split it across statements"),
            Self::PrintfWidth(width) => write!(f, "awk: printf width is {width}; the limit is 4096"),
            Self::PrintfPrecision(precision) => write!(f, "awk: printf precision is {precision}; the limit is 4096"),
            Self::RegexExtension => f.write_str("awk: `(?` is not ERE syntax; awk regexes are POSIX extended regular expressions"),
            Self::InternalCaptures(count) => write!(f, "awk: internal refusal: a compiled regex has {count} capture groups instead of one"),
            Self::ParserStart(cause) => write!(f, "awk: could not start the parser: {cause}"),
            Self::Internal(cause) => write!(f, "awk: internal refusal: {cause}"),
        }
    }
}

impl fmt::Display for AwkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => refusal.fmt(f),
            Self::Syntax {
                line,
                column,
                expected,
            } => write!(
                f,
                "awk: syntax error at line {line}, column {column}: {expected}"
            ),
            Self::Runtime { error, record } => match error {
                RuntimeError::Regex(_) | RuntimeError::Format(_) => {
                    write!(f, "{error} (record {record})")
                }
                RuntimeError::DivisionByZero
                | RuntimeError::FieldIndex(_)
                | RuntimeError::FieldCount
                | RuntimeError::InvalidUtf8
                | RuntimeError::EmptySeparator => write!(f, "awk: {error} (record {record})"),
            },
            Self::Limit(limit) => write!(f, "{limit:?}"),
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DivisionByZero => f.write_str("division by zero"),
            Self::FieldIndex(index) => write!(f, "field index {index} is out of range"),
            Self::FieldCount => f.write_str("more than 65 536 fields"),
            Self::InvalidUtf8 => f.write_str("record is not valid UTF-8"),
            Self::EmptySeparator => f.write_str("an empty field separator is not supported"),
            Self::Regex(message) => f.write_str(message),
            Self::Format(refusal) => refusal.fmt(f),
        }
    }
}

impl AwkError {
    pub(super) fn regex(failure: CommandFailure) -> Self {
        match failure {
            CommandFailure::Status { message, status: _ } => Self::Refused(Refusal::Regex(message)),
            CommandFailure::Fatal(FatalError::Limit(limit)) => Self::Limit(limit),
            CommandFailure::Fatal(
                FatalError::Unsupported(cause) | FatalError::Assertion(cause),
            ) => Self::Refused(Refusal::Internal(cause)),
        }
    }
}
impl From<Refusal> for AwkError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}
impl From<LimitExceeded> for AwkError {
    fn from(limit: LimitExceeded) -> Self {
        Self::Limit(limit)
    }
}
impl From<AwkError> for CommandFailure {
    fn from(error: AwkError) -> Self {
        match error {
            AwkError::Limit(limit) => limit.into(),
            error @ (AwkError::Refused(_) | AwkError::Syntax { .. } | AwkError::Runtime { .. }) => {
                Self::usage(error.to_string())
            }
        }
    }
}

pub(super) fn name_refusal(name: &str) -> Option<Refusal> {
    if let Some(variable) = GawkVariable::lookup(name) {
        return Some(Refusal::GawkVariable(variable));
    }
    if let Some(syntax) = GawkSyntax::lookup(name) {
        return Some(Refusal::GawkSyntax(syntax));
    }
    match name {
        "getline" => Some(Refusal::Getline),
        "function" | "func" => Some(Refusal::UserFunction),
        "RS" => Some(Refusal::RecordSeparator),
        "OFMT" | "CONVFMT" => Some(Refusal::NumberFormat(name.into())),
        "ENVIRON" | "ARGV" | "ARGC" | "FILENAME" => Some(Refusal::Environment(name.into())),
        "SUBSEP" => Some(Refusal::MultiSubscript),
        "nextfile" => Some(Refusal::Nextfile),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Function {
    Length,
    Substr,
    Index,
    Split,
    Sub,
    Gsub,
    Match,
    Sprintf,
    Tolower,
    Toupper,
    Int,
    Sqrt,
    Exp,
    Log,
    Sin,
    Cos,
    Atan2,
}
impl Function {
    fn from_name(name: &str) -> Result<Self, Refusal> {
        match name {
            "length" => Ok(Self::Length),
            "substr" => Ok(Self::Substr),
            "index" => Ok(Self::Index),
            "split" => Ok(Self::Split),
            "sub" => Ok(Self::Sub),
            "gsub" => Ok(Self::Gsub),
            "match" => Ok(Self::Match),
            "sprintf" => Ok(Self::Sprintf),
            "tolower" => Ok(Self::Tolower),
            "toupper" => Ok(Self::Toupper),
            "int" => Ok(Self::Int),
            "sqrt" => Ok(Self::Sqrt),
            "exp" => Ok(Self::Exp),
            "log" => Ok(Self::Log),
            "sin" => Ok(Self::Sin),
            "cos" => Ok(Self::Cos),
            "atan2" => Ok(Self::Atan2),
            "system" => Err(Refusal::Process),
            "close" | "fflush" => Err(Refusal::StreamFunction(name.into())),
            "rand" | "srand" => Err(Refusal::Randomness(name.into())),
            _ => Err(Refusal::UnknownFunction(name.into())),
        }
    }
    fn accepts(self, count: usize) -> bool {
        match self {
            Self::Length => count <= 1,
            Self::Substr | Self::Split | Self::Sub | Self::Gsub => (2..=3).contains(&count),
            Self::Index | Self::Match | Self::Atan2 => count == 2,
            Self::Sprintf => count >= 1,
            Self::Tolower
            | Self::Toupper
            | Self::Int
            | Self::Sqrt
            | Self::Exp
            | Self::Log
            | Self::Sin
            | Self::Cos => count == 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Unary {
    Not,
    Plus,
    Minus,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Increment {
    PreAdd,
    PreSub,
    PostAdd,
    PostSub,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Binary {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    ModAssign,
    PowAssign,
    Or,
    And,
    Match,
    NotMatch,
    Lt,
    Le,
    Eq,
    Ne,
    Gt,
    Ge,
    Concat,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}
#[derive(Debug, PartialEq)]
pub(super) struct Expr {
    pub kind: ExprKind,
    pub height: u8,
}
#[derive(Debug, PartialEq)]
pub(super) enum ExprKind {
    Number(f64),
    String(String),
    Regex(usize),
    Variable(String),
    Array(String, Box<Expr>),
    Field(Box<Expr>),
    Group(Box<Expr>),
    Call(Function, Vec<Expr>),
    Unary(Unary, Box<Expr>),
    Increment(Increment, Box<Expr>),
    Binary(Binary, Box<Expr>, Box<Expr>),
    Chain(Box<Expr>, Vec<(Binary, Expr)>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    In(Box<Expr>, String),
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum PrintKind {
    Print,
    Printf,
}
#[derive(Debug, PartialEq)]
pub(super) struct Stmt {
    pub kind: StmtKind,
    pub height: u8,
}
#[derive(Debug, PartialEq)]
pub(super) enum StmtKind {
    Empty,
    Block(Vec<Stmt>),
    Expr(Expr),
    Print(PrintKind, Vec<Expr>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    Do(Box<Stmt>, Expr),
    For(Option<Expr>, Option<Expr>, Option<Expr>, Box<Stmt>),
    ForIn(String, String, Box<Stmt>),
    Break,
    Continue,
    Next,
    Exit(Option<Expr>),
    Delete(String, Option<Expr>),
}
#[derive(Debug, PartialEq)]
pub(super) enum Pattern {
    Always,
    Expr(Expr),
    Range(Expr, Expr),
}
#[derive(Debug, PartialEq)]
pub(super) struct Rule {
    pub pattern: Pattern,
    pub action: Stmt,
}
#[derive(Debug, PartialEq)]
pub(super) struct Program {
    pub begin: Vec<Stmt>,
    pub rules: Vec<Rule>,
    pub end: Vec<Stmt>,
    pub regexes: Vec<String>,
}

impl Expr {
    fn new(kind: ExprKind, children: u8) -> Result<Self, AwkError> {
        Ok(Self {
            kind,
            height: height(children)?,
        })
    }
    fn lvalue(&self) -> bool {
        match &self.kind {
            ExprKind::Variable(_) | ExprKind::Array(_, _) | ExprKind::Field(_) => true,
            ExprKind::Group(inner) => inner.lvalue(),
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Regex(_)
            | ExprKind::Call(_, _)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment(_, _)
            | ExprKind::Binary(_, _, _)
            | ExprKind::Chain(_, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _) => false,
        }
    }
}
fn height(children: u8) -> Result<u8, AwkError> {
    if children >= 32 {
        Err(Refusal::AstHeight.into())
    } else {
        Ok(children + 1)
    }
}
fn deeper(depth: u8) -> Result<u8, AwkError> {
    if depth >= 96 {
        Err(Refusal::ParseDepth.into())
    } else {
        Ok(depth + 1)
    }
}

pub(super) fn program(source: &str) -> Result<Program, AwkError> {
    let mut lexer = Lexer::new(source);
    let token = lexer.next()?;
    let mut parser = Parser {
        lexer,
        token,
        regexes: Vec::new(),
        context: Context::Rule,
        loops: 0,
    };
    let mut program = Program {
        begin: Vec::new(),
        rules: Vec::new(),
        end: Vec::new(),
        regexes: Vec::new(),
    };
    parser.separators()?;
    while parser.token.kind != TokenKind::Eof {
        if parser.name() == Some("BEGIN") || parser.name() == Some("END") {
            let begin = parser.name() == Some("BEGIN");
            parser.advance()?;
            parser.context = if begin { Context::Begin } else { Context::End };
            if !parser.at(Punct::LBrace) {
                return parser.error("an action after BEGIN or END");
            }
            let action = parser.statement(0)?;
            if begin {
                program.begin.push(action);
            } else {
                program.end.push(action);
            }
        } else {
            parser.context = Context::Rule;
            let pattern = if parser.at(Punct::LBrace) {
                Pattern::Always
            } else {
                let first = parser.expression(0, 0, ExprContext::Normal)?;
                if parser.eat(Punct::Comma)? {
                    parser.newlines()?;
                    Pattern::Range(first, parser.expression(0, 0, ExprContext::Normal)?)
                } else {
                    Pattern::Expr(first)
                }
            };
            let action = if parser.at(Punct::LBrace) {
                parser.statement(0)?
            } else {
                if !parser.terminated() {
                    return parser.error("an action or a rule separator");
                }
                Stmt {
                    kind: StmtKind::Print(PrintKind::Print, Vec::new()),
                    height: 1,
                }
            };
            program.rules.push(Rule { pattern, action });
        }
        parser.separators()?;
    }
    program.regexes = parser.regexes;
    check_names(&program)?;
    Ok(program)
}

#[derive(Clone, Copy, PartialEq)]
enum Context {
    Begin,
    Rule,
    End,
}
#[derive(Clone, Copy, PartialEq)]
enum ExprContext {
    Normal,
    Print,
}
struct Parser<'a> {
    lexer: Lexer<'a>,
    token: Token,
    regexes: Vec<String>,
    context: Context,
    loops: u32,
}

impl Parser<'_> {
    fn advance(&mut self) -> Result<(), AwkError> {
        self.token = self.lexer.next()?;
        Ok(())
    }
    fn at(&self, punct: Punct) -> bool {
        self.token.kind == TokenKind::Punct(punct)
    }
    fn eat(&mut self, punct: Punct) -> Result<bool, AwkError> {
        if !self.at(punct) {
            return Ok(false);
        }
        self.advance()?;
        Ok(true)
    }
    fn require(&mut self, punct: Punct) -> Result<(), AwkError> {
        if self.eat(punct)? {
            Ok(())
        } else {
            self.error(format!("{punct:?}"))
        }
    }
    fn error<T>(&self, expected: impl Into<String>) -> Result<T, AwkError> {
        Err(AwkError::Syntax {
            line: self.token.line,
            column: self.token.column,
            expected: expected.into(),
        })
    }
    fn name(&self) -> Option<&str> {
        match &self.token.kind {
            TokenKind::Name(name) => Some(name),
            TokenKind::Number(_)
            | TokenKind::String(_)
            | TokenKind::Punct(_)
            | TokenKind::Newline
            | TokenKind::Eof => None,
        }
    }
    fn take_name(&mut self) -> Result<String, AwkError> {
        let name = self
            .name()
            .ok_or_else(|| AwkError::Syntax {
                line: self.token.line,
                column: self.token.column,
                expected: "an identifier".into(),
            })?
            .to_owned();
        self.advance()?;
        Ok(name)
    }
    fn newlines(&mut self) -> Result<(), AwkError> {
        while self.token.kind == TokenKind::Newline {
            self.advance()?;
        }
        Ok(())
    }
    fn separators(&mut self) -> Result<(), AwkError> {
        while self.token.kind == TokenKind::Newline || self.at(Punct::Semi) {
            self.advance()?;
        }
        Ok(())
    }
    fn terminated(&self) -> bool {
        self.token.kind == TokenKind::Eof
            || self.token.kind == TokenKind::Newline
            || self.at(Punct::Semi)
            || self.at(Punct::RBrace)
            || self.name() == Some("else")
    }
    fn statement(&mut self, depth: u8) -> Result<Stmt, AwkError> {
        let depth = deeper(depth)?;
        let kind = if self.eat(Punct::LBrace)? {
            let mut statements = Vec::new();
            self.newlines()?;
            while !self.at(Punct::RBrace) {
                if self.token.kind == TokenKind::Eof {
                    return self.error("a closing action brace");
                }
                statements.push(self.statement(depth)?);
                self.separators()?;
            }
            self.advance()?;
            StmtKind::Block(statements)
        } else if self.eat(Punct::Semi)? {
            StmtKind::Empty
        } else {
            let keyword = self.name().unwrap_or("").to_owned();
            match keyword.as_str() {
                "if" => {
                    self.advance()?;
                    let condition = self.condition(depth)?;
                    let then = self.statement(depth)?;
                    self.eat(Punct::Semi)?;
                    self.newlines()?;
                    let otherwise = if self.name() == Some("else") {
                        self.advance()?;
                        self.newlines()?;
                        Some(Box::new(self.statement(depth)?))
                    } else {
                        None
                    };
                    StmtKind::If(condition, Box::new(then), otherwise)
                }
                "while" => {
                    self.advance()?;
                    let condition = self.condition(depth)?;
                    self.loops += 1;
                    let body = self.statement(depth)?;
                    self.loops -= 1;
                    StmtKind::While(condition, Box::new(body))
                }
                "do" => {
                    self.advance()?;
                    self.newlines()?;
                    self.loops += 1;
                    let body = self.statement(depth)?;
                    self.loops -= 1;
                    self.eat(Punct::Semi)?;
                    self.newlines()?;
                    if self.name() != Some("while") {
                        return self.error("while after a do body");
                    }
                    self.advance()?;
                    let condition = self.condition(depth)?;
                    StmtKind::Do(Box::new(body), condition)
                }
                "for" => return self.for_statement(depth),
                "print" | "printf" => {
                    let kind = if keyword == "print" {
                        PrintKind::Print
                    } else {
                        PrintKind::Printf
                    };
                    self.advance()?;
                    StmtKind::Print(kind, self.print_list(kind, depth)?)
                }
                "break" | "continue" => {
                    if self.loops == 0 {
                        return self.error("break or continue inside a loop");
                    }
                    self.advance()?;
                    if keyword == "break" {
                        StmtKind::Break
                    } else {
                        StmtKind::Continue
                    }
                }
                "next" => {
                    match self.context {
                        Context::Begin | Context::End => {
                            return Err(Refusal::NextOutsideRule.into());
                        }
                        Context::Rule => self.advance()?,
                    }
                    StmtKind::Next
                }
                "exit" => {
                    self.advance()?;
                    StmtKind::Exit(if self.terminated() {
                        None
                    } else {
                        Some(self.expression(0, depth, ExprContext::Normal)?)
                    })
                }
                "delete" => {
                    self.advance()?;
                    let name = self.take_name()?;
                    let key = if self.eat(Punct::LBracket)? {
                        let key = self.expression(0, depth, ExprContext::Normal)?;
                        if self.at(Punct::Comma) {
                            return Err(Refusal::MultiSubscript.into());
                        }
                        self.require(Punct::RBracket)?;
                        Some(key)
                    } else {
                        None
                    };
                    StmtKind::Delete(name, key)
                }
                _ => StmtKind::Expr(self.expression(0, depth, ExprContext::Normal)?),
            }
        };
        let children = statement_height(&kind);
        match &kind {
            StmtKind::Empty
            | StmtKind::Block(_)
            | StmtKind::If(_, _, _)
            | StmtKind::While(_, _)
            | StmtKind::Do(_, _)
            | StmtKind::For(_, _, _, _)
            | StmtKind::ForIn(_, _, _) => {}
            StmtKind::Expr(_)
            | StmtKind::Print(_, _)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Next
            | StmtKind::Exit(_)
            | StmtKind::Delete(_, _) => {
                if !self.terminated() {
                    return self.error("a statement separator");
                }
            }
        }
        Ok(Stmt {
            kind,
            height: height(children)?,
        })
    }
    fn condition(&mut self, depth: u8) -> Result<Expr, AwkError> {
        self.require(Punct::LParen)?;
        let condition = self.expression(0, depth, ExprContext::Normal)?;
        self.require(Punct::RParen)?;
        self.newlines()?;
        Ok(condition)
    }
    fn for_statement(&mut self, depth: u8) -> Result<Stmt, AwkError> {
        let depth = deeper(depth)?;
        self.advance()?;
        self.require(Punct::LParen)?;
        let init = self.optional_expression(Punct::Semi, depth)?;
        let kind = if self.eat(Punct::RParen)? {
            let Some(expr) = init else {
                return self.error("a for-in header");
            };
            let (key, array) = match expr.kind {
                ExprKind::In(key, array) => (key, array),
                ExprKind::Number(_)
                | ExprKind::String(_)
                | ExprKind::Regex(_)
                | ExprKind::Variable(_)
                | ExprKind::Array(_, _)
                | ExprKind::Field(_)
                | ExprKind::Group(_)
                | ExprKind::Call(_, _)
                | ExprKind::Unary(_, _)
                | ExprKind::Increment(_, _)
                | ExprKind::Binary(_, _, _)
                | ExprKind::Chain(_, _)
                | ExprKind::Conditional(_, _, _) => {
                    return self.error("for (name in array) or a three-part for header");
                }
            };
            let key = match key.kind {
                ExprKind::Variable(key) => key,
                ExprKind::Number(_)
                | ExprKind::String(_)
                | ExprKind::Regex(_)
                | ExprKind::Array(_, _)
                | ExprKind::Field(_)
                | ExprKind::Group(_)
                | ExprKind::Call(_, _)
                | ExprKind::Unary(_, _)
                | ExprKind::Increment(_, _)
                | ExprKind::Binary(_, _, _)
                | ExprKind::Chain(_, _)
                | ExprKind::Conditional(_, _, _)
                | ExprKind::In(_, _) => return self.error("a variable before in"),
            };
            self.newlines()?;
            self.loops += 1;
            let body = self.statement(depth)?;
            self.loops -= 1;
            StmtKind::ForIn(key, array, Box::new(body))
        } else {
            self.require(Punct::Semi)?;
            let condition = self.optional_expression(Punct::Semi, depth)?;
            self.require(Punct::Semi)?;
            let step = self.optional_expression(Punct::RParen, depth)?;
            self.require(Punct::RParen)?;
            self.newlines()?;
            self.loops += 1;
            let body = self.statement(depth)?;
            self.loops -= 1;
            StmtKind::For(init, condition, step, Box::new(body))
        };
        let children = statement_height(&kind);
        Ok(Stmt {
            kind,
            height: height(children)?,
        })
    }
    fn optional_expression(&mut self, end: Punct, depth: u8) -> Result<Option<Expr>, AwkError> {
        if self.at(end) {
            Ok(None)
        } else {
            self.expression(0, depth, ExprContext::Normal).map(Some)
        }
    }
    fn print_list(&mut self, kind: PrintKind, depth: u8) -> Result<Vec<Expr>, AwkError> {
        let mut list = Vec::new();
        if !self.terminated() {
            if self.eat(Punct::LParen)? {
                let first = self.expression(0, deeper(depth)?, ExprContext::Normal)?;
                list.push(first);
                while self.eat(Punct::Comma)? {
                    self.newlines()?;
                    list.push(self.expression(0, deeper(depth)?, ExprContext::Normal)?);
                }
                self.require(Punct::RParen)?;
                if list.len() == 1 {
                    let first = list.remove(0);
                    let h = first.height;
                    let group = Expr::new(ExprKind::Group(Box::new(first)), h)?;
                    list.push(self.tail(group, 0, deeper(depth)?, ExprContext::Print)?);
                }
            } else {
                list.push(self.expression(0, depth, ExprContext::Print)?);
            }
            while self.eat(Punct::Comma)? {
                self.newlines()?;
                list.push(self.expression(0, depth, ExprContext::Print)?);
            }
        }
        if self.at(Punct::Gt) || self.at(Punct::Append) || self.at(Punct::Pipe) {
            return Err(Refusal::OutputRedirection.into());
        }
        if kind == PrintKind::Printf {
            let Some(format) = list.first() else {
                return self.error("a printf format");
            };
            if let Some(format) = literal_string(format) {
                check_format(format)?;
            }
        }
        Ok(list)
    }
    fn expression(
        &mut self,
        minimum: u8,
        depth: u8,
        context: ExprContext,
    ) -> Result<Expr, AwkError> {
        let depth = deeper(depth)?;
        let left = self.prefix(depth)?;
        self.tail(left, minimum, depth, context)
    }
    fn named_expression(&mut self, name: String, depth: u8) -> Result<Expr, AwkError> {
        let depth = deeper(depth)?;
        if [
            "BEGIN", "END", "print", "printf", "if", "else", "while", "do", "for", "break",
            "continue", "next", "exit", "delete", "in",
        ]
        .contains(&name.as_str())
        {
            return self.error("an expression");
        }
        let line = self.token.line;
        let column = self.token.column + name.chars().count();
        self.advance()?;
        if self.at(Punct::LParen)
            && (Function::from_name(&name).is_ok()
                || ["system", "close", "fflush", "rand", "srand"].contains(&name.as_str())
                || (line == self.token.line && column == self.token.column))
        {
            let function = Function::from_name(&name)?;
            self.advance()?;
            let mut arguments = Vec::new();
            if !self.at(Punct::RParen) {
                loop {
                    arguments.push(self.expression(0, depth, ExprContext::Normal)?);
                    if !self.eat(Punct::Comma)? {
                        break;
                    }
                    self.newlines()?;
                }
            }
            self.require(Punct::RParen)?;
            if !function.accepts(arguments.len()) {
                return self.error(format!("the arguments required by {name}()"));
            }
            if function == Function::Sprintf
                && let Some(format) = arguments.first().and_then(literal_string)
            {
                check_format(format)?;
            }
            let h = arguments.iter().map(|arg| arg.height).max().unwrap_or(0);
            Expr::new(ExprKind::Call(function, arguments), h)
        } else if self.eat(Punct::LBracket)? {
            let key = self.expression(0, depth, ExprContext::Normal)?;
            if self.at(Punct::Comma) {
                return Err(Refusal::MultiSubscript.into());
            }
            self.require(Punct::RBracket)?;
            let h = key.height;
            Expr::new(ExprKind::Array(name, Box::new(key)), h)
        } else if name == "length" {
            Expr::new(ExprKind::Call(Function::Length, Vec::new()), 0)
        } else {
            Expr::new(ExprKind::Variable(name), 0)
        }
    }
    fn prefix(&mut self, depth: u8) -> Result<Expr, AwkError> {
        let depth = deeper(depth)?;
        let token = std::mem::replace(&mut self.token.kind, TokenKind::Eof);
        match token {
            TokenKind::Number(value) => {
                self.advance()?;
                Expr::new(ExprKind::Number(value), 0)
            }
            TokenKind::String(value) => {
                self.advance()?;
                Expr::new(ExprKind::String(value), 0)
            }
            TokenKind::Name(name) => self.named_expression(name, depth),
            TokenKind::Punct(Punct::LParen) => {
                self.advance()?;
                let inner = self.expression(0, depth, ExprContext::Normal)?;
                if self.at(Punct::Comma) {
                    return Err(Refusal::MultiSubscript.into());
                }
                self.require(Punct::RParen)?;
                let h = inner.height;
                Expr::new(ExprKind::Group(Box::new(inner)), h)
            }
            TokenKind::Punct(Punct::Div | Punct::DivAssign) => {
                let prefix = if token == TokenKind::Punct(Punct::DivAssign) {
                    "="
                } else {
                    ""
                };
                let source = self.lexer.regex(prefix)?;
                self.advance()?;
                let index = self.regexes.len();
                self.regexes.push(source);
                Expr::new(ExprKind::Regex(index), 0)
            }
            TokenKind::Punct(
                Punct::Not | Punct::Add | Punct::Sub | Punct::Dollar | Punct::Inc | Punct::Dec,
            ) => {
                let punct = token;
                self.advance()?;
                let minimum = if punct == TokenKind::Punct(Punct::Dollar) {
                    14
                } else if punct == TokenKind::Punct(Punct::Inc)
                    || punct == TokenKind::Punct(Punct::Dec)
                {
                    13
                } else {
                    11
                };
                let operand = self.expression(minimum, depth, ExprContext::Normal)?;
                let h = operand.height;
                let kind = if punct == TokenKind::Punct(Punct::Dollar) {
                    ExprKind::Field(Box::new(operand))
                } else if punct == TokenKind::Punct(Punct::Inc)
                    || punct == TokenKind::Punct(Punct::Dec)
                {
                    if !operand.lvalue() {
                        return self.error("an assignable increment operand");
                    }
                    ExprKind::Increment(
                        if punct == TokenKind::Punct(Punct::Inc) {
                            Increment::PreAdd
                        } else {
                            Increment::PreSub
                        },
                        Box::new(operand),
                    )
                } else {
                    let unary = if punct == TokenKind::Punct(Punct::Not) {
                        Unary::Not
                    } else if punct == TokenKind::Punct(Punct::Add) {
                        Unary::Plus
                    } else {
                        Unary::Minus
                    };
                    ExprKind::Unary(unary, Box::new(operand))
                };
                Expr::new(kind, h)
            }
            TokenKind::Punct(
                Punct::LBrace
                | Punct::RBrace
                | Punct::RParen
                | Punct::LBracket
                | Punct::RBracket
                | Punct::Semi
                | Punct::Comma
                | Punct::Question
                | Punct::Colon
                | Punct::Assign
                | Punct::AddAssign
                | Punct::SubAssign
                | Punct::MulAssign
                | Punct::ModAssign
                | Punct::PowAssign
                | Punct::Or
                | Punct::And
                | Punct::Match
                | Punct::NotMatch
                | Punct::Lt
                | Punct::Le
                | Punct::Eq
                | Punct::Ne
                | Punct::Gt
                | Punct::Ge
                | Punct::Mul
                | Punct::Mod
                | Punct::Pow
                | Punct::Append
                | Punct::Pipe,
            )
            | TokenKind::Newline
            | TokenKind::Eof => self.error("an expression"),
        }
    }
    fn tail(
        &mut self,
        mut left: Expr,
        minimum: u8,
        depth: u8,
        context: ExprContext,
    ) -> Result<Expr, AwkError> {
        let depth = deeper(depth)?;
        loop {
            if minimum <= 13 && left.lvalue() && (self.at(Punct::Inc) || self.at(Punct::Dec)) {
                let op = if self.at(Punct::Inc) {
                    Increment::PostAdd
                } else {
                    Increment::PostSub
                };
                self.advance()?;
                let h = left.height;
                left = Expr::new(ExprKind::Increment(op, Box::new(left)), h)?;
                continue;
            }
            if minimum <= 2 && self.eat(Punct::Question)? {
                let yes = self.expression(0, depth, context)?;
                self.require(Punct::Colon)?;
                let no = self.expression(1, depth, context)?;
                let h = left.height.max(yes.height).max(no.height);
                left = Expr::new(
                    ExprKind::Conditional(Box::new(left), Box::new(yes), Box::new(no)),
                    h,
                )?;
                continue;
            }
            if minimum <= 5 && self.name() == Some("in") {
                self.advance()?;
                let array = self.take_name()?;
                let h = left.height;
                left = Expr::new(ExprKind::In(Box::new(left), array), h)?;
                continue;
            }
            if context == ExprContext::Normal && self.at(Punct::Pipe) {
                self.advance()?;
                return self.error("getline after an input pipe");
            }
            let explicit = operators()
                .iter()
                .find(|(punct, _, _)| self.at(*punct))
                .copied();
            let operator = if let Some((_, op, binding)) = explicit {
                Some((op, binding))
            } else if starts_concat(&self.token.kind) {
                Some((Binary::Concat, 8))
            } else {
                None
            };
            let Some((op, binding)) = operator else {
                break;
            };
            if binding < minimum {
                break;
            }
            if context == ExprContext::Print {
                if op == Binary::Gt {
                    return Err(Refusal::OutputRedirection.into());
                }
                if let Some(text) = comparison(op) {
                    return Err(Refusal::PrintComparison(text.into()).into());
                }
            }
            if (binding == 6 || binding == 7) && non_associative(&left, binding) {
                return self.error("a parenthesized comparison or match");
            }
            let right_associative = binding == 1 || op == Binary::Pow;
            if binding == 1 && !left.lvalue() {
                return self.error("an assignable expression");
            }
            if explicit.is_some() {
                self.advance()?;
            }
            if op == Binary::And || op == Binary::Or {
                self.newlines()?;
            }
            let right = self.expression(
                if right_associative {
                    binding
                } else {
                    binding + 1
                },
                depth,
                context,
            )?;
            if op == Binary::Assign
                && let ExprKind::Variable(name) = &left.kind
                && name == "FS"
                && literal_string(&right) == Some("")
            {
                return Err(Refusal::EmptySeparator.into());
            }
            let h = left.height.max(right.height);
            if [
                Binary::Concat,
                Binary::Add,
                Binary::Sub,
                Binary::Mul,
                Binary::Div,
                Binary::Mod,
                Binary::And,
                Binary::Or,
            ]
            .contains(&op)
            {
                if let ExprKind::Chain(_, rest) = &mut left.kind
                    && rest
                        .last()
                        .is_some_and(|(previous, _)| precedence(*previous) == binding)
                {
                    left.height = left.height.max(height(right.height)?);
                    rest.push((op, right));
                } else {
                    left = Expr::new(ExprKind::Chain(Box::new(left), vec![(op, right)]), h)?;
                }
            } else {
                left = Expr::new(ExprKind::Binary(op, Box::new(left), Box::new(right)), h)?;
            }
        }
        Ok(left)
    }
}

fn operators() -> &'static [(Punct, Binary, u8)] {
    &[
        (Punct::Assign, Binary::Assign, 1),
        (Punct::AddAssign, Binary::AddAssign, 1),
        (Punct::SubAssign, Binary::SubAssign, 1),
        (Punct::MulAssign, Binary::MulAssign, 1),
        (Punct::DivAssign, Binary::DivAssign, 1),
        (Punct::ModAssign, Binary::ModAssign, 1),
        (Punct::PowAssign, Binary::PowAssign, 1),
        (Punct::Or, Binary::Or, 3),
        (Punct::And, Binary::And, 4),
        (Punct::Match, Binary::Match, 6),
        (Punct::NotMatch, Binary::NotMatch, 6),
        (Punct::Lt, Binary::Lt, 7),
        (Punct::Le, Binary::Le, 7),
        (Punct::Eq, Binary::Eq, 7),
        (Punct::Ne, Binary::Ne, 7),
        (Punct::Gt, Binary::Gt, 7),
        (Punct::Ge, Binary::Ge, 7),
        (Punct::Add, Binary::Add, 9),
        (Punct::Sub, Binary::Sub, 9),
        (Punct::Mul, Binary::Mul, 10),
        (Punct::Div, Binary::Div, 10),
        (Punct::Mod, Binary::Mod, 10),
        (Punct::Pow, Binary::Pow, 12),
    ]
}
fn precedence(op: Binary) -> u8 {
    if op == Binary::Concat {
        8
    } else {
        operators()
            .iter()
            .find(|(_, candidate, _)| *candidate == op)
            .map_or(0, |(_, _, binding)| *binding)
    }
}
fn comparison(op: Binary) -> Option<&'static str> {
    [
        (Binary::Lt, "<"),
        (Binary::Le, "<="),
        (Binary::Eq, "=="),
        (Binary::Ne, "!="),
        (Binary::Gt, ">"),
        (Binary::Ge, ">="),
    ]
    .into_iter()
    .find(|(candidate, _)| *candidate == op)
    .map(|(_, text)| text)
}
fn non_associative(expr: &Expr, binding: u8) -> bool {
    match &expr.kind {
        ExprKind::Binary(op, _, _) => precedence(*op) == binding,
        ExprKind::Number(_)
        | ExprKind::String(_)
        | ExprKind::Regex(_)
        | ExprKind::Variable(_)
        | ExprKind::Array(_, _)
        | ExprKind::Field(_)
        | ExprKind::Group(_)
        | ExprKind::Call(_, _)
        | ExprKind::Unary(_, _)
        | ExprKind::Increment(_, _)
        | ExprKind::Chain(_, _)
        | ExprKind::Conditional(_, _, _)
        | ExprKind::In(_, _) => false,
    }
}
fn starts_concat(token: &TokenKind) -> bool {
    match token {
        TokenKind::Number(_) | TokenKind::String(_) => true,
        TokenKind::Name(name) => ![
            "BEGIN", "END", "print", "printf", "if", "else", "while", "do", "for", "break",
            "continue", "next", "exit", "delete", "in",
        ]
        .contains(&name.as_str()),
        TokenKind::Punct(punct) => [
            Punct::LParen,
            Punct::Dollar,
            Punct::Not,
            Punct::Inc,
            Punct::Dec,
        ]
        .contains(punct),
        TokenKind::Newline | TokenKind::Eof => false,
    }
}
fn literal_string(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::String(text) => Some(text),
        ExprKind::Group(inner) => literal_string(inner),
        ExprKind::Number(_)
        | ExprKind::Regex(_)
        | ExprKind::Variable(_)
        | ExprKind::Array(_, _)
        | ExprKind::Field(_)
        | ExprKind::Call(_, _)
        | ExprKind::Unary(_, _)
        | ExprKind::Increment(_, _)
        | ExprKind::Binary(_, _, _)
        | ExprKind::Chain(_, _)
        | ExprKind::Conditional(_, _, _)
        | ExprKind::In(_, _) => None,
    }
}
fn statement_height(stmt: &StmtKind) -> u8 {
    match stmt {
        StmtKind::Empty | StmtKind::Break | StmtKind::Continue | StmtKind::Next => 0,
        StmtKind::Block(stmts) => stmts.iter().map(|s| s.height).max().unwrap_or(0),
        StmtKind::Expr(expr) => expr.height,
        StmtKind::Print(_, args) => args.iter().map(|e| e.height).max().unwrap_or(0),
        StmtKind::If(condition, yes, no) => condition
            .height
            .max(yes.height)
            .max(no.as_ref().map_or(0, |s| s.height)),
        StmtKind::While(condition, body) | StmtKind::Do(body, condition) => {
            condition.height.max(body.height)
        }
        StmtKind::For(init, condition, step, body) => init
            .iter()
            .chain(condition)
            .chain(step)
            .map(|e| e.height)
            .max()
            .unwrap_or(0)
            .max(body.height),
        StmtKind::ForIn(_, _, body) => body.height,
        StmtKind::Exit(expr) | StmtKind::Delete(_, expr) => expr.as_ref().map_or(0, |e| e.height),
    }
}

pub(super) fn check_format(format: &str) -> Result<(), AwkError> {
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            continue;
        }
        while chars.peek().is_some_and(|ch| "-+ #0".contains(*ch)) {
            chars.next();
        }
        if chars.peek() == Some(&'*') {
            return Err(Refusal::PrintfStar.into());
        }
        let width = format_number(&mut chars);
        if width > 4096 {
            return Err(Refusal::PrintfWidth(width).into());
        }
        if chars.peek() == Some(&'.') {
            chars.next();
            if chars.peek() == Some(&'*') {
                return Err(Refusal::PrintfStar.into());
            }
            let precision = format_number(&mut chars);
            if precision > 4096 {
                return Err(Refusal::PrintfPrecision(precision).into());
            }
        }
        let Some(conversion) = chars.next() else {
            return Err(AwkError::Syntax {
                line: 1,
                column: format.chars().count() + 1,
                expected: "a printf conversion after %".into(),
            });
        };
        if !"cdiouxXeEfFgGs%".contains(conversion) {
            return Err(Refusal::PrintfConversion(conversion).into());
        }
    }
    Ok(())
}
fn format_number(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> usize {
    let mut number = 0usize;
    while let Some(ch @ '0'..='9') = chars.peek().copied() {
        chars.next();
        number = number
            .saturating_mul(10)
            .saturating_add(ch as usize - '0' as usize);
    }
    number
}

#[derive(Clone, Copy, PartialEq)]
enum Use {
    Scalar,
    Array,
}
#[derive(Default)]
struct Names {
    used: BTreeMap<String, Use>,
}
impl Names {
    fn name(&mut self, name: &str, usage: Use) -> Result<(), AwkError> {
        if let Some(previous) = self.used.insert(name.into(), usage)
            && previous != usage
        {
            return Err(Refusal::ScalarArray(name.into()).into());
        }
        Ok(())
    }
    fn expr(&mut self, expr: &Expr) -> Result<(), AwkError> {
        match &expr.kind {
            ExprKind::Number(_) | ExprKind::String(_) | ExprKind::Regex(_) => Ok(()),
            ExprKind::Variable(name) => self.name(name, Use::Scalar),
            ExprKind::Array(name, key) => {
                self.name(name, Use::Array)?;
                self.expr(key)
            }
            ExprKind::Field(inner)
            | ExprKind::Group(inner)
            | ExprKind::Unary(_, inner)
            | ExprKind::Increment(_, inner) => self.expr(inner),
            ExprKind::Binary(_, left, right) => {
                self.expr(left)?;
                self.expr(right)
            }
            ExprKind::Chain(first, rest) => {
                self.expr(first)?;
                for (_, expr) in rest {
                    self.expr(expr)?;
                }
                Ok(())
            }
            ExprKind::Conditional(condition, yes, no) => {
                self.expr(condition)?;
                self.expr(yes)?;
                self.expr(no)
            }
            ExprKind::In(key, array) => {
                self.name(array, Use::Array)?;
                self.expr(key)
            }
            ExprKind::Call(function, arguments) => self.call(*function, arguments),
        }
    }
    fn call(&mut self, function: Function, arguments: &[Expr]) -> Result<(), AwkError> {
        for (index, expr) in arguments.iter().enumerate() {
            if function == Function::Length && variable(expr).is_some() {
                continue;
            }
            if function == Function::Split && index == 1 {
                let name = variable(expr).ok_or_else(|| AwkError::Syntax {
                    line: 1,
                    column: 1,
                    expected: "an array name as split's second argument".into(),
                })?;
                self.name(name, Use::Array)?;
            } else {
                if (function == Function::Sub || function == Function::Gsub)
                    && index == 2
                    && !expr.lvalue()
                {
                    return Err(AwkError::Syntax {
                        line: 1,
                        column: 1,
                        expected: "an assignable sub/gsub target".into(),
                    });
                }
                self.expr(expr)?;
            }
        }
        Ok(())
    }
    fn stmt(&mut self, stmt: &Stmt) -> Result<(), AwkError> {
        match &stmt.kind {
            StmtKind::Empty | StmtKind::Break | StmtKind::Continue | StmtKind::Next => Ok(()),
            StmtKind::Block(statements) => {
                for stmt in statements {
                    self.stmt(stmt)?;
                }
                Ok(())
            }
            StmtKind::Expr(expr) => self.expr(expr),
            StmtKind::Print(_, args) => {
                for expr in args {
                    self.expr(expr)?;
                }
                Ok(())
            }
            StmtKind::If(condition, yes, no) => {
                self.expr(condition)?;
                self.stmt(yes)?;
                if let Some(no) = no {
                    self.stmt(no)?;
                }
                Ok(())
            }
            StmtKind::While(condition, body) | StmtKind::Do(body, condition) => {
                self.expr(condition)?;
                self.stmt(body)
            }
            StmtKind::For(init, condition, step, body) => {
                for expr in init.iter().chain(condition).chain(step) {
                    self.expr(expr)?;
                }
                self.stmt(body)
            }
            StmtKind::ForIn(key, array, body) => {
                self.name(key, Use::Scalar)?;
                self.name(array, Use::Array)?;
                self.stmt(body)
            }
            StmtKind::Exit(expr) => {
                if let Some(expr) = expr {
                    self.expr(expr)?;
                }
                Ok(())
            }
            StmtKind::Delete(array, key) => {
                self.name(array, Use::Array)?;
                if let Some(key) = key {
                    self.expr(key)?;
                }
                Ok(())
            }
        }
    }
}
fn variable(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Variable(name) => Some(name),
        ExprKind::Group(inner) => variable(inner),
        ExprKind::Number(_)
        | ExprKind::String(_)
        | ExprKind::Regex(_)
        | ExprKind::Array(_, _)
        | ExprKind::Field(_)
        | ExprKind::Call(_, _)
        | ExprKind::Unary(_, _)
        | ExprKind::Increment(_, _)
        | ExprKind::Binary(_, _, _)
        | ExprKind::Chain(_, _)
        | ExprKind::Conditional(_, _, _)
        | ExprKind::In(_, _) => None,
    }
}
fn check_names(program: &Program) -> Result<(), AwkError> {
    let mut names = Names::default();
    for name in ["NF", "NR", "FNR", "FS", "OFS", "ORS", "RSTART", "RLENGTH"] {
        names.name(name, Use::Scalar)?;
    }
    for stmt in &program.begin {
        names.stmt(stmt)?;
    }
    for rule in &program.rules {
        match &rule.pattern {
            Pattern::Always => {}
            Pattern::Expr(expr) => names.expr(expr)?,
            Pattern::Range(start, end) => {
                names.expr(start)?;
                names.expr(end)?;
            }
        }
        names.stmt(&rule.action)?;
    }
    for stmt in &program.end {
        names.stmt(stmt)?;
    }
    Ok(())
}
