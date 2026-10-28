use super::{
    AwkError,
    lex::{Kind, Lexer, Symbol, Token},
};
use crate::builtins::{CommandFailure, unsupported_flag};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq)]
pub(super) enum GawkVariable {
    IgnoreCase,
    Fpat,
    FieldWidths,
    ProcInfo,
    Rt,
    Binmode,
    Lint,
    TextDomain,
    Errno,
}

impl GawkVariable {
    fn name(&self) -> &str {
        match self {
            Self::IgnoreCase => "IGNORECASE",
            Self::Fpat => "FPAT",
            Self::FieldWidths => "FIELDWIDTHS",
            Self::ProcInfo => "PROCINFO",
            Self::Rt => "RT",
            Self::Binmode => "BINMODE",
            Self::Lint => "LINT",
            Self::TextDomain => "TEXTDOMAIN",
            Self::Errno => "ERRNO",
        }
    }
    fn advice(&self) -> &str {
        match self {
            Self::IgnoreCase => "for case-insensitive matching compare `tolower($0) ~ /re/`",
            Self::Fpat
            | Self::FieldWidths
            | Self::ProcInfo
            | Self::Rt
            | Self::Binmode
            | Self::Lint
            | Self::TextDomain
            | Self::Errno => "use FS, split() or substr()",
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum GawkSyntax {
    BeginFile,
    EndFile,
    Include,
    Load,
    Switch,
}

impl GawkSyntax {
    fn name(&self) -> &str {
        match self {
            Self::BeginFile => "BEGINFILE",
            Self::EndFile => "ENDFILE",
            Self::Include => "@include",
            Self::Load => "@load",
            Self::Switch => "switch",
        }
    }
    fn advice(&self) -> &str {
        match self {
            Self::Switch => "use if/else",
            Self::BeginFile | Self::EndFile | Self::Include | Self::Load => "awk has one input",
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum Refusal {
    Getline,
    Process,
    Redirection,
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
    BadVariable(String),
    BadFieldRegex(CommandFailure),
    EmptySeparator,
    FormatStar,
    FormatConversion(char),
    FormatBound {
        function: String,
        dimension: String,
        value: String,
    },
    ProgramBytes(usize),
    ParseDepth,
    AstHeight,
    RegexExtension,
    Regex(CommandFailure),
    ParserStart(String),
    ParserPanic(String),
    InternalRegex,
}

fn shared_message(error: &CommandFailure) -> String {
    match error {
        CommandFailure::Status { message, status: _ } => message.clone(),
        CommandFailure::Fatal(fatal) => format!("awk: internal regex failure: {fatal:?}"),
    }
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Self::Getline => "awk: `getline` is not supported: awk reads only its one input, one record per rule pass; write a rule per record, or join inputs with the shell before the pipe".into(),
            Self::Process => "awk: `system()` is not supported: this shell runs no processes; run the command in the script and pipe its output into awk".into(),
            Self::Redirection => "awk: output redirection is not supported: awk writes only to its stdout; redirect the whole command (`awk '…' > buf`, `awk '…' >&2`), or parenthesize a comparison: `print (a > b)`".into(),
            Self::StreamFunction(name) => format!("awk: `{name}()` is not supported: awk opens no files or pipes here"),
            Self::Randomness(name) => format!("awk: `{name}()` is not supported: the shell has no clock or entropy source"),
            Self::UserFunction => "awk: user-defined functions are not supported: write the expression inline, or call awk from a shell function".into(),
            Self::UnknownFunction(name) => format!("awk: `{name}()` is not a function here; the built-in functions are length substr index split sub gsub match sprintf tolower toupper int sqrt exp log sin cos atan2"),
            Self::RecordSeparator => "awk: `RS` is not supported: records are always lines; split a record yourself with `split($0, parts, sep)`".into(),
            Self::NumberFormat(name) => format!("awk: `{name}` is not supported: numbers print as integers or with %.6g; use printf or sprintf for another format"),
            Self::Environment(name) => format!("awk: `{name}` is not supported: awk sees only its input and its -v assignments; pass values with `-v name=value`"),
            Self::GawkVariable(name) => format!("awk: `{}` is a gawk variable and is not supported; {}", name.name(), name.advice()),
            Self::MultiSubscript => "awk: multi-dimensional subscripts are not supported: join the keys yourself, `a[i \",\" j]`".into(),
            Self::PowerSpelling => "awk: `**` is not supported: POSIX awk spells power `^`".into(),
            Self::Nextfile => "awk: `nextfile` is not supported: awk has one input; use `exit` to stop reading".into(),
            Self::GawkSyntax(name) => format!("awk: `{}` is a gawk extension and is not supported; {}", name.name(), name.advice()),
            Self::PrintComparison(operator) => format!("awk: an unparenthesized comparison in a print list is not POSIX awk; write `print (a {operator} b)`"),
            Self::NextOutsideRule => "awk: `next` is only allowed in a rule, not in BEGIN or END".into(),
            Self::ScalarArray(name) => format!("awk: `{name}` is used both as an array and as a scalar"),
            Self::Option(flag) => shared_message(&unsupported_flag("awk", flag, "-F SEP -v NAME=VALUE")),
            Self::OperandAssignment(value) => format!("awk: operand assignment `{value}` is not supported: use `-v {value}` before the program"),
            Self::SecondInput => "awk: at most one input operand (a named buffer or `-`); join buffers with `cat a b | awk '…'`".into(),
            Self::NoProgram => "awk: a program argument is required".into(),
            Self::BadVariable(value) => format!("awk: -v needs NAME=VALUE with NAME an identifier, got `{value}`"),
            Self::BadFieldRegex(error) | Self::Regex(error) => shared_message(error),
            Self::EmptySeparator => "awk: an empty field separator is not supported; take characters with `substr($0, i, 1)`".into(),
            Self::FormatStar => "awk: printf `*` width is not supported; build the format with sprintf or concatenation".into(),
            Self::FormatConversion(c) => format!("awk: printf conversion `%{c}` is not supported; use %c %d %i %o %u %x %X %e %E %f %F %g %G %s or %%"),
            Self::FormatBound { function, dimension, value } => format!("awk: `{function}` {dimension} {value} is more than 4096; use a {dimension} of at most 4096"),
            Self::ProgramBytes(n) => format!("awk: program is {n} bytes; the limit is 8192"),
            Self::ParseDepth => "awk: program nests too deeply (more than 96 parser levels); split it across statements".into(),
            Self::AstHeight => "awk: program nests deeper than 32 levels; split it across statements".into(),
            Self::RegexExtension => "awk: `(?` is not ERE syntax; awk regexes are POSIX extended regular expressions".into(),
            Self::ParserStart(reason) => format!("awk: could not start the parser: {reason}"),
            Self::ParserPanic(reason) => format!("awk: internal parser failure: {reason}"),
            Self::InternalRegex => "awk: internal regex failure: captures remained after non-capturing rewriting".into(),
        }
    }
}

pub(super) fn check_name(name: &str) -> Result<(), Refusal> {
    let refusal = match name {
        "getline" => Refusal::Getline,
        "function" | "func" => Refusal::UserFunction,
        "RS" => Refusal::RecordSeparator,
        "OFMT" | "CONVFMT" => Refusal::NumberFormat(name.into()),
        "ENVIRON" | "ARGV" | "ARGC" | "FILENAME" => Refusal::Environment(name.into()),
        "IGNORECASE" => Refusal::GawkVariable(GawkVariable::IgnoreCase),
        "FPAT" => Refusal::GawkVariable(GawkVariable::Fpat),
        "FIELDWIDTHS" => Refusal::GawkVariable(GawkVariable::FieldWidths),
        "PROCINFO" => Refusal::GawkVariable(GawkVariable::ProcInfo),
        "RT" => Refusal::GawkVariable(GawkVariable::Rt),
        "BINMODE" => Refusal::GawkVariable(GawkVariable::Binmode),
        "LINT" => Refusal::GawkVariable(GawkVariable::Lint),
        "TEXTDOMAIN" => Refusal::GawkVariable(GawkVariable::TextDomain),
        "ERRNO" => Refusal::GawkVariable(GawkVariable::Errno),
        "SUBSEP" => Refusal::MultiSubscript,
        "nextfile" => Refusal::Nextfile,
        "BEGINFILE" => Refusal::GawkSyntax(GawkSyntax::BeginFile),
        "ENDFILE" => Refusal::GawkSyntax(GawkSyntax::EndFile),
        "switch" => Refusal::GawkSyntax(GawkSyntax::Switch),
        _ => return Ok(()),
    };
    Err(refusal)
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
    fn lookup(name: &str) -> Result<Self, Refusal> {
        Ok(match name {
            "length" => Self::Length,
            "substr" => Self::Substr,
            "index" => Self::Index,
            "split" => Self::Split,
            "sub" => Self::Sub,
            "gsub" => Self::Gsub,
            "match" => Self::Match,
            "sprintf" => Self::Sprintf,
            "tolower" => Self::Tolower,
            "toupper" => Self::Toupper,
            "int" => Self::Int,
            "sqrt" => Self::Sqrt,
            "exp" => Self::Exp,
            "log" => Self::Log,
            "sin" => Self::Sin,
            "cos" => Self::Cos,
            "atan2" => Self::Atan2,
            "system" => return Err(Refusal::Process),
            "close" | "fflush" => return Err(Refusal::StreamFunction(name.into())),
            "rand" | "srand" => return Err(Refusal::Randomness(name.into())),
            _ => return Err(Refusal::UnknownFunction(name.into())),
        })
    }
    fn arity(self) -> std::ops::RangeInclusive<usize> {
        match self {
            Self::Length => 0..=1,
            Self::Substr | Self::Split | Self::Sub | Self::Gsub => 2..=3,
            Self::Index | Self::Match | Self::Atan2 => 2..=2,
            Self::Sprintf => 1..=usize::MAX,
            Self::Tolower
            | Self::Toupper
            | Self::Int
            | Self::Sqrt
            | Self::Exp
            | Self::Log
            | Self::Sin
            | Self::Cos => 1..=1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Binary {
    Or,
    And,
    Match,
    NotMatch,
    Lt,
    Le,
    Ne,
    Eq,
    Gt,
    Ge,
    Concat,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
}

impl Binary {
    fn precedence(self) -> u8 {
        match self {
            Self::Or => 3,
            Self::And => 4,
            Self::Match | Self::NotMatch => 6,
            Self::Lt | Self::Le | Self::Ne | Self::Eq | Self::Gt | Self::Ge => 7,
            Self::Concat => 8,
            Self::Add | Self::Sub => 9,
            Self::Mul | Self::Div | Self::Rem => 10,
            Self::Pow => 12,
        }
    }
    fn chain(self) -> bool {
        match self {
            Self::Or
            | Self::And
            | Self::Concat
            | Self::Add
            | Self::Sub
            | Self::Mul
            | Self::Div
            | Self::Rem => true,
            Self::Match
            | Self::NotMatch
            | Self::Lt
            | Self::Le
            | Self::Ne
            | Self::Eq
            | Self::Gt
            | Self::Ge
            | Self::Pow => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Unary {
    Not,
    Plus,
    Minus,
}

#[derive(Debug, PartialEq)]
pub(super) struct Expr {
    pub kind: ExprKind,
    pub height: u8,
    line: usize,
    column: usize,
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
    Unary(Unary, Box<Expr>),
    Increment {
        target: Box<Expr>,
        delta: i8,
        post: bool,
    },
    Binary(Binary, Box<Expr>, Box<Expr>),
    Chain {
        first: Box<Expr>,
        rest: Vec<(Binary, Expr)>,
    },
    Assign(Option<Binary>, Box<Expr>, Box<Expr>),
    Conditional(Box<Expr>, Box<Expr>, Box<Expr>),
    In(Box<Expr>, String),
    Call(Function, Vec<Expr>),
}

impl Expr {
    fn ungrouped(&self) -> &Self {
        match &self.kind {
            ExprKind::Group(inner) => inner.ungrouped(),
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Regex(_)
            | ExprKind::Variable(_)
            | ExprKind::Array(_, _)
            | ExprKind::Field(_)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment { .. }
            | ExprKind::Binary(_, _, _)
            | ExprKind::Chain { .. }
            | ExprKind::Assign(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)
            | ExprKind::Call(_, _) => self,
        }
    }
    fn new(kind: ExprKind, height: u8) -> Result<Self, AwkError> {
        if height > 32 {
            return Err(Refusal::AstHeight.into());
        }
        Ok(Self {
            kind,
            height,
            line: 1,
            column: 1,
        })
    }
    fn error(&self, expected: &str) -> AwkError {
        AwkError::Syntax {
            line: self.line,
            column: self.column,
            expected: expected.into(),
        }
    }
    fn lvalue(&self) -> bool {
        match &self.kind {
            ExprKind::Variable(_) | ExprKind::Array(_, _) | ExprKind::Field(_) => true,
            ExprKind::Group(e) => e.lvalue(),
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Regex(_)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment { .. }
            | ExprKind::Binary(_, _, _)
            | ExprKind::Chain { .. }
            | ExprKind::Assign(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)
            | ExprKind::Call(_, _) => false,
        }
    }
    fn variable(&self) -> Option<&str> {
        match &self.kind {
            ExprKind::Variable(name) => Some(name),
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Regex(_)
            | ExprKind::Array(_, _)
            | ExprKind::Field(_)
            | ExprKind::Group(_)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment { .. }
            | ExprKind::Binary(_, _, _)
            | ExprKind::Chain { .. }
            | ExprKind::Assign(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)
            | ExprKind::Call(_, _) => None,
        }
    }
    fn literal_string(&self) -> Option<&str> {
        match &self.kind {
            ExprKind::String(value) => Some(value),
            ExprKind::Group(inner) => inner.literal_string(),
            ExprKind::Number(_)
            | ExprKind::Regex(_)
            | ExprKind::Variable(_)
            | ExprKind::Array(_, _)
            | ExprKind::Field(_)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment { .. }
            | ExprKind::Binary(_, _, _)
            | ExprKind::Chain { .. }
            | ExprKind::Assign(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)
            | ExprKind::Call(_, _) => None,
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct Stmt {
    pub kind: StmtKind,
    pub height: u8,
}

#[derive(Debug, PartialEq)]
pub(super) enum StmtKind {
    Block(Vec<Stmt>),
    Empty,
    Expression(Expr),
    Print(Vec<Expr>),
    Printf(Vec<Expr>),
    If {
        condition: Expr,
        yes: Box<Stmt>,
        no: Option<Box<Stmt>>,
    },
    While(Expr, Box<Stmt>),
    Do(Box<Stmt>, Expr),
    For {
        init: Option<Expr>,
        condition: Option<Expr>,
        step: Option<Expr>,
        body: Box<Stmt>,
    },
    ForIn {
        key: String,
        array: String,
        body: Box<Stmt>,
    },
    Break,
    Continue,
    Next,
    Exit(Option<Expr>),
    Delete(String, Option<Expr>),
}

impl Stmt {
    fn needs_separator(&self) -> bool {
        match &self.kind {
            StmtKind::Block(_) | StmtKind::Empty => false,
            StmtKind::If { yes, no, .. } => no.as_ref().unwrap_or(yes).needs_separator(),
            StmtKind::While(_, body)
            | StmtKind::For { body, .. }
            | StmtKind::ForIn { body, .. } => body.needs_separator(),
            StmtKind::Expression(_)
            | StmtKind::Print(_)
            | StmtKind::Printf(_)
            | StmtKind::Do(_, _)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Next
            | StmtKind::Exit(_)
            | StmtKind::Delete(_, _) => true,
        }
    }
    fn new(kind: StmtKind, child_height: u8) -> Result<Self, AwkError> {
        let height = child_height + 1;
        if height > 32 {
            return Err(Refusal::AstHeight.into());
        }
        Ok(Self { kind, height })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Phase {
    Begin,
    Record,
    End,
}

#[derive(Debug, PartialEq)]
pub(super) struct Rule {
    pub phase: Phase,
    pub pattern: Option<Expr>,
    pub range_end: Option<Expr>,
    pub action: Option<Stmt>,
}

#[derive(Debug, PartialEq)]
pub(super) struct Program {
    pub rules: Vec<Rule>,
    pub regex_sources: Vec<String>,
    pub arrays: std::collections::BTreeSet<String>,
}

struct Parser<'a> {
    lexer: Lexer<'a>,
    look: Option<Token>,
    regex_sources: Vec<String>,
    phase: Phase,
    loops: usize,
}

pub(super) fn program(source: &str) -> Result<Program, AwkError> {
    let mut parser = Parser {
        lexer: Lexer::new(source),
        look: None,
        regex_sources: vec![],
        phase: Phase::Record,
        loops: 0,
    };
    let mut rules = Vec::new();
    parser.separators()?;
    while parser.peek()?.kind != Kind::End {
        let rule = parser.rule()?;
        let ended_action = rule.action.is_some();
        rules.push(rule);
        let next = parser.peek()?;
        if !ended_action
            && next.kind != Kind::End
            && next.kind != Kind::Newline
            && next.kind != Kind::Symbol(Symbol::Semi)
        {
            return Err(next.error("a newline or `;` between rules"));
        }
        parser.separators()?;
    }
    let mut program = Program {
        rules,
        regex_sources: parser.regex_sources,
        arrays: std::collections::BTreeSet::new(),
    };
    program.arrays = check_uses(&program)?;
    Ok(program)
}

impl Parser<'_> {
    fn peek(&mut self) -> Result<&Token, AwkError> {
        if self.look.is_none() {
            self.look = Some(self.lexer.next()?);
        }
        self.look
            .as_ref()
            .ok_or_else(|| AwkError::Refused(Refusal::ParserPanic("missing lookahead".into())))
    }
    fn take(&mut self) -> Result<Token, AwkError> {
        match self.look.take() {
            Some(token) => Ok(token),
            None => self.lexer.next(),
        }
    }
    fn at(&mut self, symbol: Symbol) -> Result<bool, AwkError> {
        Ok(self.peek()?.kind == Kind::Symbol(symbol))
    }
    fn eat(&mut self, symbol: Symbol) -> Result<bool, AwkError> {
        if !self.at(symbol)? {
            return Ok(false);
        }
        self.take()?;
        Ok(true)
    }
    fn expect(&mut self, symbol: Symbol) -> Result<(), AwkError> {
        if self.eat(symbol)? {
            Ok(())
        } else {
            Err(self.peek()?.error(format!("{symbol:?}")))
        }
    }
    fn named(&mut self, name: &str) -> Result<bool, AwkError> {
        Ok(self.peek()?.kind == Kind::Name(name.into()))
    }
    fn newlines(&mut self) -> Result<(), AwkError> {
        while self.peek()?.kind == Kind::Newline {
            self.take()?;
        }
        Ok(())
    }
    fn separators(&mut self) -> Result<(), AwkError> {
        while self.peek()?.kind == Kind::Newline || self.at(Symbol::Semi)? {
            self.take()?;
        }
        Ok(())
    }
    fn name(&mut self) -> Result<String, AwkError> {
        let token = self.take()?;
        match &token.kind {
            Kind::Name(name) => {
                check_name(name)?;
                if keyword(name) || Function::lookup(name).is_ok() {
                    return Err(token.error("an array identifier"));
                }
                Ok(name.clone())
            }
            Kind::Number(_) | Kind::String(_) | Kind::Symbol(_) | Kind::Newline | Kind::End => {
                Err(token.error("an identifier"))
            }
        }
    }
    fn depth(depth: u8) -> Result<u8, AwkError> {
        if depth == 96 {
            Err(Refusal::ParseDepth.into())
        } else {
            Ok(depth + 1)
        }
    }
    fn rule(&mut self) -> Result<Rule, AwkError> {
        let phase = if self.named("BEGIN")? {
            self.take()?;
            Phase::Begin
        } else if self.named("END")? {
            self.take()?;
            Phase::End
        } else {
            Phase::Record
        };
        self.phase = phase;
        if phase != Phase::Record {
            self.newlines()?;
            if !self.at(Symbol::Brace)? {
                return Err(self.peek()?.error("an action after BEGIN or END"));
            }
            return Ok(Rule {
                phase,
                pattern: None,
                range_end: None,
                action: Some(self.statement(0)?),
            });
        }
        let pattern = if self.at(Symbol::Brace)? {
            None
        } else {
            Some(self.expression(1, 0, false)?)
        };
        let range_end = if self.eat(Symbol::Comma)? {
            self.newlines()?;
            Some(self.expression(1, 0, false)?)
        } else {
            None
        };
        let action = if self.at(Symbol::Brace)? {
            Some(self.statement(0)?)
        } else {
            None
        };
        Ok(Rule {
            phase,
            pattern,
            range_end,
            action,
        })
    }

    fn expression(&mut self, minimum: u8, depth: u8, print_list: bool) -> Result<Expr, AwkError> {
        let depth = Self::depth(depth)?;
        let token = self.take()?;
        let left = match &token.kind {
            Kind::Number(n) => Expr::new(ExprKind::Number(*n), 1)?,
            Kind::String(s) => Expr::new(ExprKind::String(s.clone()), 1)?,
            Kind::Name(name) => self.primary_name(name, &token, depth)?,
            Kind::Symbol(symbol) => self.primary_symbol(*symbol, &token, depth, print_list)?,
            Kind::Newline | Kind::End => return Err(token.error("an expression")),
        };
        let mut expression = self.expression_tail(left, minimum, depth, print_list)?;
        expression.line = token.line;
        expression.column = token.column;
        Ok(expression)
    }

    fn primary_name(&mut self, name: &str, token: &Token, depth: u8) -> Result<Expr, AwkError> {
        let depth = Self::depth(depth)?;
        check_name(name)?;
        if keyword(name) {
            return Err(token.error("an expression"));
        }
        let function = Function::lookup(name);
        let reserved_call = ["system", "close", "fflush", "rand", "srand"].contains(&name);
        if self.at(Symbol::Open)?
            && (self.peek()?.start == token.end || function.is_ok() || reserved_call)
        {
            let function = function?;
            self.take()?;
            let args = self.expression_list(Symbol::Close, depth, false)?;
            if !function.arity().contains(&args.len()) {
                return Err(token.error(format!("the arguments to `{name}`")));
            }
            let height = args.iter().map(|e| e.height).max().unwrap_or(0) + 1;
            return Expr::new(ExprKind::Call(function, args), height);
        }
        if name == "length" {
            return Expr::new(ExprKind::Call(Function::Length, vec![]), 1);
        }
        if function.is_ok() {
            return Err(token.error("a parenthesized function argument list"));
        }
        if self.eat(Symbol::Bracket)? {
            let index = self.expression(1, depth, false)?;
            if self.at(Symbol::Comma)? {
                return Err(Refusal::MultiSubscript.into());
            }
            self.expect(Symbol::EndBracket)?;
            let height = index.height + 1;
            Expr::new(ExprKind::Array(name.into(), Box::new(index)), height)
        } else {
            Expr::new(ExprKind::Variable(name.into()), 1)
        }
    }

    fn primary_symbol(
        &mut self,
        symbol: Symbol,
        token: &Token,
        depth: u8,
        print_list: bool,
    ) -> Result<Expr, AwkError> {
        let depth = Self::depth(depth)?;
        use Symbol as S;
        match symbol {
            S::Div | S::DivAssign => {
                let source = self
                    .lexer
                    .regex(token, if symbol == S::DivAssign { "=" } else { "" })?;
                let index = self.regex_sources.len();
                self.regex_sources.push(source);
                Expr::new(ExprKind::Regex(index), 1)
            }
            S::Open => {
                let expr = self.expression(1, depth, false)?;
                if self.at(S::Comma)? {
                    return Err(Refusal::MultiSubscript.into());
                }
                self.expect(S::Close)?;
                let height = expr.height + 1;
                Expr::new(ExprKind::Group(Box::new(expr)), height)
            }
            S::Dollar => {
                let expr = self.expression(14, depth, print_list)?;
                let height = expr.height + 1;
                Expr::new(ExprKind::Field(Box::new(expr)), height)
            }
            S::Not | S::Add | S::Sub => {
                let expr = self.expression(11, depth, print_list)?;
                let unary = if symbol == S::Not {
                    Unary::Not
                } else if symbol == S::Add {
                    Unary::Plus
                } else {
                    Unary::Minus
                };
                let height = expr.height + 1;
                Expr::new(ExprKind::Unary(unary, Box::new(expr)), height)
            }
            S::Increment | S::Decrement => {
                let expr = self.expression(13, depth, print_list)?;
                increment(expr, symbol, false, token)
            }
            S::Close
            | S::Brace
            | S::EndBrace
            | S::Bracket
            | S::EndBracket
            | S::Comma
            | S::Semi
            | S::Mul
            | S::Rem
            | S::Pow
            | S::Match
            | S::NotMatch
            | S::Lt
            | S::Le
            | S::Eq
            | S::Ne
            | S::Gt
            | S::Ge
            | S::And
            | S::Or
            | S::Question
            | S::Colon
            | S::Assign
            | S::AddAssign
            | S::SubAssign
            | S::MulAssign
            | S::RemAssign
            | S::PowAssign
            | S::Append
            | S::Pipe => Err(token.error("an expression")),
        }
    }

    fn expression_tail(
        &mut self,
        mut left: Expr,
        minimum: u8,
        depth: u8,
        print_list: bool,
    ) -> Result<Expr, AwkError> {
        let depth = Self::depth(depth)?;
        loop {
            let token = self.peek()?.clone();
            if minimum <= 13
                && (token.kind == Kind::Symbol(Symbol::Increment)
                    || token.kind == Kind::Symbol(Symbol::Decrement))
            {
                self.take()?;
                left = increment(
                    left,
                    if token.kind == Kind::Symbol(Symbol::Increment) {
                        Symbol::Increment
                    } else {
                        Symbol::Decrement
                    },
                    true,
                    &token,
                )?;
                continue;
            }
            let Some(operator) = infix(&token.kind) else {
                break;
            };
            if operator.precedence() < minimum {
                break;
            }
            if print_list {
                operator.check_print()?;
            }
            if operator != Infix::Binary(Binary::Concat) {
                self.take()?;
            }
            match operator {
                Infix::Conditional => {
                    let yes = self.expression(1, depth, print_list)?;
                    self.expect(Symbol::Colon)?;
                    let no = self.expression(2, depth, print_list)?;
                    let height = left.height.max(yes.height).max(no.height) + 1;
                    left = Expr::new(
                        ExprKind::Conditional(Box::new(left), Box::new(yes), Box::new(no)),
                        height,
                    )?;
                }
                Infix::In => {
                    let array = self.name()?;
                    let height = left.height + 1;
                    left = Expr::new(ExprKind::In(Box::new(left), array), height)?;
                }
                Infix::Assign(operation) => {
                    if !left.lvalue() {
                        return Err(token.error("an assignable variable, field or array element"));
                    }
                    let right = self.expression(1, depth, print_list)?;
                    if operation.is_none()
                        && left.ungrouped().variable() == Some("FS")
                        && right.literal_string() == Some("")
                    {
                        return Err(Refusal::EmptySeparator.into());
                    }
                    let height = left.height.max(right.height) + 1;
                    left = Expr::new(
                        ExprKind::Assign(operation, Box::new(left), Box::new(right)),
                        height,
                    )?;
                }
                Infix::Binary(operation) => {
                    if operation == Binary::And || operation == Binary::Or {
                        self.newlines()?;
                    }
                    let precedence = operation.precedence();
                    if (6..=7).contains(&precedence) && left.non_associative(precedence) {
                        return Err(
                            token.error("parentheses around chained comparisons or matches")
                        );
                    }
                    let right = self.expression(
                        precedence + u8::from(operation != Binary::Pow),
                        depth,
                        print_list,
                    )?;
                    left = combine(left, operation, right)?;
                }
                Infix::Redirection => {
                    if self.named("getline")? {
                        return Err(Refusal::Getline.into());
                    }
                    return Err(token.error("an awk expression without a pipe or redirection"));
                }
            }
        }
        Ok(left)
    }

    fn expression_list(
        &mut self,
        end: Symbol,
        depth: u8,
        print_list: bool,
    ) -> Result<Vec<Expr>, AwkError> {
        let depth = Self::depth(depth)?;
        let mut list = Vec::new();
        if !self.at(end)? {
            loop {
                list.push(self.expression(1, depth, print_list)?);
                if !self.eat(Symbol::Comma)? {
                    break;
                }
                self.newlines()?;
            }
        }
        self.expect(end)?;
        Ok(list)
    }
}

impl Expr {
    fn non_associative(&self, precedence: u8) -> bool {
        match &self.kind {
            ExprKind::Binary(operator, _, _) => operator.precedence() == precedence,
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Regex(_)
            | ExprKind::Variable(_)
            | ExprKind::Array(_, _)
            | ExprKind::Field(_)
            | ExprKind::Group(_)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment { .. }
            | ExprKind::Chain { .. }
            | ExprKind::Assign(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)
            | ExprKind::Call(_, _) => false,
        }
    }
}

fn increment(target: Expr, symbol: Symbol, post: bool, token: &Token) -> Result<Expr, AwkError> {
    if !target.lvalue() {
        return Err(token.error("an assignable increment operand"));
    }
    let height = target.height + 1;
    Expr::new(
        ExprKind::Increment {
            target: Box::new(target),
            delta: if symbol == Symbol::Increment { 1 } else { -1 },
            post,
        },
        height,
    )
}

fn combine(left: Expr, operator: Binary, right: Expr) -> Result<Expr, AwkError> {
    if operator.chain() {
        let same = match &left.kind {
            ExprKind::Chain { rest, .. } => rest
                .first()
                .is_some_and(|(op, _)| op.precedence() == operator.precedence()),
            ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::Regex(_)
            | ExprKind::Variable(_)
            | ExprKind::Array(_, _)
            | ExprKind::Field(_)
            | ExprKind::Group(_)
            | ExprKind::Unary(_, _)
            | ExprKind::Increment { .. }
            | ExprKind::Binary(_, _, _)
            | ExprKind::Assign(_, _, _)
            | ExprKind::Conditional(_, _, _)
            | ExprKind::In(_, _)
            | ExprKind::Call(_, _) => false,
        };
        if same {
            let height = left.height.max(right.height + 1);
            if let ExprKind::Chain { first, mut rest } = left.kind {
                rest.push((operator, right));
                return Expr::new(ExprKind::Chain { first, rest }, height);
            }
        }
        let height = left.height.max(right.height) + 1;
        return Expr::new(
            ExprKind::Chain {
                first: Box::new(left),
                rest: vec![(operator, right)],
            },
            height,
        );
    }
    let height = left.height.max(right.height) + 1;
    Expr::new(
        ExprKind::Binary(operator, Box::new(left), Box::new(right)),
        height,
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Infix {
    Binary(Binary),
    Assign(Option<Binary>),
    In,
    Conditional,
    Redirection,
}

impl Infix {
    fn precedence(self) -> u8 {
        match self {
            Self::Binary(op) => op.precedence(),
            Self::Assign(_) => 1,
            Self::In => 5,
            Self::Conditional => 2,
            Self::Redirection => 1,
        }
    }
    fn check_print(self) -> Result<(), Refusal> {
        match self {
            Self::Binary(Binary::Gt) | Self::Redirection => Err(Refusal::Redirection),
            Self::Binary(Binary::Lt) => Err(Refusal::PrintComparison("<".into())),
            Self::Binary(Binary::Le) => Err(Refusal::PrintComparison("<=".into())),
            Self::Binary(Binary::Eq) => Err(Refusal::PrintComparison("==".into())),
            Self::Binary(Binary::Ne) => Err(Refusal::PrintComparison("!=".into())),
            Self::Binary(Binary::Ge) => Err(Refusal::PrintComparison(">=".into())),
            Self::Binary(
                Binary::Or
                | Binary::And
                | Binary::Match
                | Binary::NotMatch
                | Binary::Concat
                | Binary::Add
                | Binary::Sub
                | Binary::Mul
                | Binary::Div
                | Binary::Rem
                | Binary::Pow,
            )
            | Self::Assign(_)
            | Self::In
            | Self::Conditional => Ok(()),
        }
    }
}

fn infix(kind: &Kind) -> Option<Infix> {
    use Symbol as S;
    Some(match kind {
        Kind::Name(name) if name == "in" => Infix::In,
        Kind::Name(name) if ["else", "while", "BEGIN", "END"].contains(&name.as_str()) => {
            return None;
        }
        Kind::Name(_) | Kind::Number(_) | Kind::String(_) => Infix::Binary(Binary::Concat),
        Kind::Newline | Kind::End => return None,
        Kind::Symbol(symbol) => match symbol {
            S::Or => Infix::Binary(Binary::Or),
            S::And => Infix::Binary(Binary::And),
            S::Match => Infix::Binary(Binary::Match),
            S::NotMatch => Infix::Binary(Binary::NotMatch),
            S::Lt => Infix::Binary(Binary::Lt),
            S::Le => Infix::Binary(Binary::Le),
            S::Eq => Infix::Binary(Binary::Eq),
            S::Ne => Infix::Binary(Binary::Ne),
            S::Gt => Infix::Binary(Binary::Gt),
            S::Ge => Infix::Binary(Binary::Ge),
            S::Add => Infix::Binary(Binary::Add),
            S::Sub => Infix::Binary(Binary::Sub),
            S::Mul => Infix::Binary(Binary::Mul),
            S::Div => Infix::Binary(Binary::Div),
            S::Rem => Infix::Binary(Binary::Rem),
            S::Pow => Infix::Binary(Binary::Pow),
            S::Assign => Infix::Assign(None),
            S::AddAssign => Infix::Assign(Some(Binary::Add)),
            S::SubAssign => Infix::Assign(Some(Binary::Sub)),
            S::MulAssign => Infix::Assign(Some(Binary::Mul)),
            S::DivAssign => Infix::Assign(Some(Binary::Div)),
            S::RemAssign => Infix::Assign(Some(Binary::Rem)),
            S::PowAssign => Infix::Assign(Some(Binary::Pow)),
            S::Question => Infix::Conditional,
            S::Pipe | S::Append => Infix::Redirection,
            S::Open | S::Dollar | S::Not => Infix::Binary(Binary::Concat),
            S::Close
            | S::Brace
            | S::EndBrace
            | S::Bracket
            | S::EndBracket
            | S::Comma
            | S::Semi
            | S::Colon
            | S::Increment
            | S::Decrement => return None,
        },
    })
}

fn keyword(name: &str) -> bool {
    [
        "BEGIN", "END", "print", "printf", "if", "else", "while", "do", "for", "in", "break",
        "continue", "next", "exit", "delete",
    ]
    .contains(&name)
}

impl Parser<'_> {
    fn statement(&mut self, depth: u8) -> Result<Stmt, AwkError> {
        let depth = Self::depth(depth)?;
        let token = self.peek()?.clone();
        if self.eat(Symbol::Brace)? {
            let mut body = Vec::new();
            self.newlines()?;
            while !self.at(Symbol::EndBrace)? {
                if self.peek()?.kind == Kind::End {
                    return Err(self.peek()?.error("a closing `}`"));
                }
                let statement = self.statement(depth)?;
                let needs_separator = statement.needs_separator();
                let empty = statement.kind == StmtKind::Empty;
                body.push(statement);
                if needs_separator && !self.terminated()? {
                    return Err(self.peek()?.error("a newline or `;` between statements"));
                }
                if !empty {
                    self.eat(Symbol::Semi)?;
                }
                self.newlines()?;
            }
            self.take()?;
            let height = body.iter().map(|s| s.height).max().unwrap_or(0);
            return Stmt::new(StmtKind::Block(body), height);
        }
        if self.eat(Symbol::Semi)? {
            return Stmt::new(StmtKind::Empty, 0);
        }
        if let Kind::Name(name) = &token.kind {
            check_name(name)?;
            match name.as_str() {
                "print" | "printf" => {
                    self.take()?;
                    return self.print(name == "printf", depth);
                }
                "if" => {
                    self.take()?;
                    return self.if_statement(depth);
                }
                "while" | "do" | "for" => {
                    self.take()?;
                    return self.loop_statement(name, depth);
                }
                "break" | "continue" => {
                    self.take()?;
                    if self.loops == 0 {
                        return Err(token.error("a loop containing break or continue"));
                    }
                    return Stmt::new(
                        if name == "break" {
                            StmtKind::Break
                        } else {
                            StmtKind::Continue
                        },
                        0,
                    );
                }
                "next" => {
                    self.take()?;
                    if self.phase != Phase::Record {
                        return Err(Refusal::NextOutsideRule.into());
                    }
                    return Stmt::new(StmtKind::Next, 0);
                }
                "exit" => {
                    self.take()?;
                    let value = if self.terminated()? {
                        None
                    } else {
                        Some(self.expression(1, depth, false)?)
                    };
                    let height = value.as_ref().map_or(0, |e| e.height);
                    return Stmt::new(StmtKind::Exit(value), height);
                }
                "delete" => {
                    self.take()?;
                    let name = self.name()?;
                    let index = if self.eat(Symbol::Bracket)? {
                        let e = self.expression(1, depth, false)?;
                        if self.at(Symbol::Comma)? {
                            return Err(Refusal::MultiSubscript.into());
                        }
                        self.expect(Symbol::EndBracket)?;
                        Some(e)
                    } else {
                        None
                    };
                    let height = index.as_ref().map_or(0, |e| e.height + 1);
                    return Stmt::new(StmtKind::Delete(name, index), height);
                }
                _ => {}
            }
        }
        let value = self.expression(1, depth, false)?;
        let height = value.height;
        Stmt::new(StmtKind::Expression(value), height)
    }

    fn terminated(&mut self) -> Result<bool, AwkError> {
        let kind = &self.peek()?.kind;
        Ok(*kind == Kind::End
            || *kind == Kind::Newline
            || *kind == Kind::Symbol(Symbol::Semi)
            || *kind == Kind::Symbol(Symbol::EndBrace)
            || *kind == Kind::Name("else".into()))
    }

    fn print(&mut self, formatted: bool, depth: u8) -> Result<Stmt, AwkError> {
        let depth = Self::depth(depth)?;
        if self.at(Symbol::Gt)? || self.at(Symbol::Append)? || self.at(Symbol::Pipe)? {
            return Err(Refusal::Redirection.into());
        }
        let mut args = Vec::new();
        let mut closed_list = false;
        if self.eat(Symbol::Open)? {
            args = self.expression_list(Symbol::Close, depth, false)?;
            if args.is_empty() {
                return Err(self.peek()?.error("an expression in the print list"));
            }
            if args.len() == 1 {
                let first = args.remove(0);
                let height = first.height + 1;
                let group = Expr::new(ExprKind::Group(Box::new(first)), height)?;
                args.push(self.expression_tail(group, 1, depth, true)?);
            } else {
                closed_list = true;
            }
        } else if !self.terminated()? {
            args.push(self.expression(1, depth, true)?);
        }
        while !closed_list && self.eat(Symbol::Comma)? {
            self.newlines()?;
            args.push(self.expression(1, depth, true)?);
        }
        if let Some(operator) = infix(&self.peek()?.kind) {
            operator.check_print()?;
        }
        if formatted && args.is_empty() {
            return Err(self.peek()?.error("a printf format"));
        }
        let height = args.iter().map(|e| e.height).max().unwrap_or(0);
        Stmt::new(
            if formatted {
                StmtKind::Printf(args)
            } else {
                StmtKind::Print(args)
            },
            height,
        )
    }

    fn condition(&mut self, depth: u8) -> Result<Expr, AwkError> {
        let depth = Self::depth(depth)?;
        self.expect(Symbol::Open)?;
        let condition = self.expression(1, depth, false)?;
        self.expect(Symbol::Close)?;
        Ok(condition)
    }

    fn body_terminator(&mut self, body: &Stmt) -> Result<bool, AwkError> {
        let terminated = !body.needs_separator()
            || self.eat(Symbol::Semi)?
            || self.peek()?.kind == Kind::Newline;
        self.newlines()?;
        Ok(terminated)
    }

    fn if_statement(&mut self, depth: u8) -> Result<Stmt, AwkError> {
        let depth = Self::depth(depth)?;
        let condition = self.condition(depth)?;
        self.newlines()?;
        let yes = Box::new(self.statement(depth)?);
        let saved = (self.lexer.clone(), self.look.clone());
        let terminated = self.body_terminator(&yes)?;
        let no = if self.named("else")? {
            if !terminated {
                return Err(self.peek()?.error("a newline or `;` before else"));
            }
            self.take()?;
            self.newlines()?;
            Some(Box::new(self.statement(depth)?))
        } else {
            self.lexer = saved.0;
            self.look = saved.1;
            None
        };
        let height = condition
            .height
            .max(yes.height)
            .max(no.as_ref().map_or(0, |s| s.height));
        Stmt::new(StmtKind::If { condition, yes, no }, height)
    }

    fn loop_body(&mut self, depth: u8) -> Result<Box<Stmt>, AwkError> {
        let depth = Self::depth(depth)?;
        self.loops += 1;
        let result = self.statement(depth).map(Box::new);
        self.loops -= 1;
        result
    }

    fn loop_statement(&mut self, name: &str, depth: u8) -> Result<Stmt, AwkError> {
        let depth = Self::depth(depth)?;
        if name == "while" {
            let condition = self.condition(depth)?;
            self.newlines()?;
            let body = self.loop_body(depth)?;
            let height = condition.height.max(body.height);
            return Stmt::new(StmtKind::While(condition, body), height);
        }
        if name == "do" {
            self.newlines()?;
            let body = self.loop_body(depth)?;
            if !self.body_terminator(&body)? {
                return Err(self.peek()?.error("a newline or `;` before while"));
            }
            if !self.named("while")? {
                return Err(self.peek()?.error("`while` after a do body"));
            }
            self.take()?;
            let condition = self.condition(depth)?;
            let height = condition.height.max(body.height);
            return Stmt::new(StmtKind::Do(body, condition), height);
        }
        self.expect(Symbol::Open)?;
        let init = if self.at(Symbol::Semi)? {
            None
        } else {
            Some(self.expression(1, depth, false)?)
        };
        if self.eat(Symbol::Close)? {
            let token = self.peek()?.clone();
            let (key, array) = for_names(init, &token)?;
            self.newlines()?;
            let body = self.loop_body(depth)?;
            let height = body.height.max(2);
            return Stmt::new(StmtKind::ForIn { key, array, body }, height);
        }
        self.expect(Symbol::Semi)?;
        self.newlines()?;
        let condition = if self.at(Symbol::Semi)? {
            None
        } else {
            Some(self.expression(1, depth, false)?)
        };
        self.expect(Symbol::Semi)?;
        self.newlines()?;
        let step = if self.at(Symbol::Close)? {
            None
        } else {
            Some(self.expression(1, depth, false)?)
        };
        self.expect(Symbol::Close)?;
        self.newlines()?;
        let body = self.loop_body(depth)?;
        let height = [&init, &condition, &step]
            .into_iter()
            .filter_map(|e| e.as_ref())
            .map(|e| e.height)
            .max()
            .unwrap_or(0)
            .max(body.height);
        Stmt::new(
            StmtKind::For {
                init,
                condition,
                step,
                body,
            },
            height,
        )
    }
}

fn for_names(init: Option<Expr>, token: &Token) -> Result<(String, String), AwkError> {
    if let Some(Expr {
        kind: ExprKind::In(key, array),
        ..
    }) = init
        && let ExprKind::Variable(key) = key.kind
    {
        return Ok((key, array));
    }
    Err(token.error("`for (key in array)` or three semicolon-separated expressions"))
}

#[derive(Clone, Copy, PartialEq)]
enum Use {
    Scalar,
    Array,
}

fn use_name(uses: &mut BTreeMap<String, Use>, name: &str, usage: Use) -> Result<(), AwkError> {
    check_name(name)?;
    if usage == Use::Array
        && ["NF", "NR", "FNR", "FS", "OFS", "ORS", "RSTART", "RLENGTH"].contains(&name)
    {
        return Err(Refusal::ScalarArray(name.into()).into());
    }
    if uses
        .insert(name.into(), usage)
        .is_some_and(|previous| previous != usage)
    {
        return Err(Refusal::ScalarArray(name.into()).into());
    }
    Ok(())
}

fn check_uses(program: &Program) -> Result<std::collections::BTreeSet<String>, AwkError> {
    let mut uses = BTreeMap::new();
    for rule in &program.rules {
        for expression in [&rule.pattern, &rule.range_end].into_iter().flatten() {
            check_expression(expression, &mut uses)?;
        }
        if let Some(action) = &rule.action {
            check_statement(action, &mut uses)?;
        }
    }
    Ok(uses
        .into_iter()
        .filter_map(|(name, usage)| match usage {
            Use::Array => Some(name),
            Use::Scalar => None,
        })
        .collect())
}

fn array_argument(expression: &Expr, uses: &mut BTreeMap<String, Use>) -> Result<(), AwkError> {
    if let Some(name) = expression.variable() {
        return use_name(uses, name, Use::Array);
    }
    Err(expression.error("an array name as split's second argument"))
}

fn check_expression(expr: &Expr, uses: &mut BTreeMap<String, Use>) -> Result<(), AwkError> {
    match &expr.kind {
        ExprKind::Number(_) | ExprKind::String(_) | ExprKind::Regex(_) => {}
        ExprKind::Variable(name) => use_name(uses, name, Use::Scalar)?,
        ExprKind::Array(name, index) => {
            use_name(uses, name, Use::Array)?;
            check_expression(index, uses)?;
        }
        ExprKind::In(key, array) => {
            check_expression(key, uses)?;
            use_name(uses, array, Use::Array)?;
        }
        ExprKind::Field(inner)
        | ExprKind::Group(inner)
        | ExprKind::Unary(_, inner)
        | ExprKind::Increment { target: inner, .. } => check_expression(inner, uses)?,
        ExprKind::Binary(_, left, right) | ExprKind::Assign(_, left, right) => {
            check_expression(left, uses)?;
            check_expression(right, uses)?;
        }
        ExprKind::Chain { first, rest } => {
            check_expression(first, uses)?;
            for (_, value) in rest {
                check_expression(value, uses)?;
            }
        }
        ExprKind::Conditional(condition, yes, no) => {
            for value in [condition, yes, no] {
                check_expression(value, uses)?;
            }
        }
        ExprKind::Call(function, args) => {
            for (index, argument) in args.iter().enumerate() {
                match function {
                    Function::Length if argument.variable().is_some() => {}
                    Function::Split if index == 1 => array_argument(argument, uses)?,
                    Function::Sub | Function::Gsub if index == 2 && !argument.lvalue() => {
                        return Err(argument.error("an assignable substitution target"));
                    }
                    Function::Length
                    | Function::Substr
                    | Function::Index
                    | Function::Split
                    | Function::Sub
                    | Function::Gsub
                    | Function::Match
                    | Function::Sprintf
                    | Function::Tolower
                    | Function::Toupper
                    | Function::Int
                    | Function::Sqrt
                    | Function::Exp
                    | Function::Log
                    | Function::Sin
                    | Function::Cos
                    | Function::Atan2 => check_expression(argument, uses)?,
                }
            }
            if *function == Function::Sprintf {
                check_literal_format("sprintf", args)?;
            }
        }
    }
    Ok(())
}

fn check_statement(statement: &Stmt, uses: &mut BTreeMap<String, Use>) -> Result<(), AwkError> {
    match &statement.kind {
        StmtKind::Block(body) => {
            for statement in body {
                check_statement(statement, uses)?;
            }
        }
        StmtKind::Empty | StmtKind::Break | StmtKind::Continue | StmtKind::Next => {}
        StmtKind::Expression(expression) => check_expression(expression, uses)?,
        StmtKind::Print(args) | StmtKind::Printf(args) => {
            for argument in args {
                check_expression(argument, uses)?;
            }
            if let StmtKind::Printf(_) = statement.kind {
                check_literal_format("printf", args)?;
            }
        }
        StmtKind::If { condition, yes, no } => {
            check_expression(condition, uses)?;
            check_statement(yes, uses)?;
            if let Some(no) = no {
                check_statement(no, uses)?;
            }
        }
        StmtKind::While(condition, body) | StmtKind::Do(body, condition) => {
            check_expression(condition, uses)?;
            check_statement(body, uses)?;
        }
        StmtKind::For {
            init,
            condition,
            step,
            body,
        } => {
            for expr in [init, condition, step].into_iter().flatten() {
                check_expression(expr, uses)?;
            }
            check_statement(body, uses)?;
        }
        StmtKind::ForIn { key, array, body } => {
            use_name(uses, key, Use::Scalar)?;
            use_name(uses, array, Use::Array)?;
            check_statement(body, uses)?;
        }
        StmtKind::Exit(value) => {
            if let Some(value) = value {
                check_expression(value, uses)?;
            }
        }
        StmtKind::Delete(array, index) => {
            use_name(uses, array, Use::Array)?;
            if let Some(index) = index {
                check_expression(index, uses)?;
            }
        }
    }
    Ok(())
}

fn check_literal_format(function: &str, args: &[Expr]) -> Result<(), AwkError> {
    if let Some(expr) = args.first() {
        check_format_expression(function, expr)?;
    }
    Ok(())
}

fn check_format_expression(function: &str, expr: &Expr) -> Result<(), AwkError> {
    match &expr.kind {
        ExprKind::String(format) => {
            super::lex::validate_format(function, format).map_err(|error| match error {
                AwkError::Syntax { expected, .. } => expr.error(&expected),
                other @ (AwkError::Refused(_) | AwkError::Runtime(_) | AwkError::Limit(_)) => other,
            })?
        }
        ExprKind::Group(expr) => check_format_expression(function, expr)?,
        ExprKind::Number(_)
        | ExprKind::Regex(_)
        | ExprKind::Variable(_)
        | ExprKind::Array(_, _)
        | ExprKind::Field(_)
        | ExprKind::Unary(_, _)
        | ExprKind::Increment { .. }
        | ExprKind::Binary(_, _, _)
        | ExprKind::Chain { .. }
        | ExprKind::Assign(_, _, _)
        | ExprKind::Conditional(_, _, _)
        | ExprKind::In(_, _)
        | ExprKind::Call(_, _) => {}
    }
    Ok(())
}
