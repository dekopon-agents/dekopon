use std::sync::Arc;

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
    Background(Background),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Background {
    pub statement: Arc<Statement>,
    pub text: Arc<str>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
    pub negated: bool,
}

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
    Buffer { append: bool, target: Word },
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
    pub patterns: Vec<Word>,
    pub body: Program,
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
    pub(crate) fn as_literal(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [WordPart::Literal(text)] => Some(text),
            _ => None,
        }
    }

    pub(crate) fn describe(&self) -> String {
        self.as_literal().map_or_else(
            || "with expansions".to_owned(),
            |literal| format!("{literal:?}"),
        )
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn is_bare_command_substitution(&self) -> bool {
        matches!(self.parts.as_slice(), [WordPart::CommandSubstitution(_)])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WordPart {
    Asset(dekopon_core::AssetNumber),
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
    LastJob,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Conditional {
    Test(ConditionalTest),
    Not(Box<Conditional>),
    And(Vec<Conditional>),
    Or(Vec<Conditional>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionalTest {
    pub words: Vec<Word>,
    pub check_right_pattern: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Pattern {
    Literal(Word),
    Expanded(Word),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArithExpr {
    node: ArithNode,
    height: Height,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Height(u32);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ArithTooDeep;

impl ArithExpr {
    pub(crate) fn integer(value: i64) -> Self {
        Self {
            node: ArithNode::Integer(value),
            height: Height(0),
        }
    }
    pub(crate) fn float(value: f64) -> Self {
        Self {
            node: ArithNode::Float(value),
            height: Height(0),
        }
    }
    pub(crate) fn variable(value: String) -> Self {
        Self {
            node: ArithNode::Variable(value),
            height: Height(0),
        }
    }
    pub(crate) fn unary(op: ArithUnaryOp, operand: Self) -> Result<Self, ArithTooDeep> {
        let height = operand.height.next()?;
        Ok(Self {
            node: ArithNode::Unary(op, Box::new(operand)),
            height,
        })
    }
    pub(crate) fn binary(op: ArithBinaryOp, left: Self, right: Self) -> Result<Self, ArithTooDeep> {
        let height = Height(left.height.0.max(right.height.0)).next()?;
        Ok(Self {
            node: ArithNode::Binary(op, Box::new(left), Box::new(right)),
            height,
        })
    }
    pub(crate) fn node(&self) -> &ArithNode {
        &self.node
    }
}

impl Height {
    fn next(self) -> Result<Self, ArithTooDeep> {
        if self.0 >= 64 {
            return Err(ArithTooDeep);
        }
        Ok(Self(self.0 + 1))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ArithNode {
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
