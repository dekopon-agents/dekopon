//! Hand-written recursive-descent parser: [`crate::lexer`] tokens to [`crate::ast`].
//!
//! Every construct the sandbox drops is rejected here by name with an actionable message. Nothing
//! is silently ignored: a script that asks for backgrounding, a subshell, a here-document, process
//! substitution, `case`, or a brace group fails to parse rather than quietly doing something else.

use thiserror::Error;

use crate::{
    ast::{
        AndOr, AndOrList, ArithBinaryOp, ArithExpr, ArithUnaryOp, Assignment, ForLoop,
        FunctionDefinition, IfStatement, Parameter, Pipeline, Program, Redirect, SimpleCommand,
        Statement, WhileLoop, Word, WordPart,
    },
    lexer::{LexError, RawParameter, RawPart, RawWord, Token, TokenKind, tokenize},
};

/// Command words this shell refuses to let a script define or invoke.
///
/// These are excluded because each one is a sandbox-escape-shaped or ambient-authority-shaped
/// feature, not because they were merely left unfinished.
pub(crate) const REJECTED_COMMANDS: &[(&str, &str)] = &[
    (
        "eval",
        "`eval` is excluded: running text the script assembled at runtime is self-modifying code and defeats the point of parsing the script up front",
    ),
    (
        "exec",
        "`exec` is excluded: this shell never replaces a process image and has no processes to replace",
    ),
    (
        "source",
        "`source` is excluded: there is no filesystem to read scripts from",
    ),
    (
        ".",
        "`.` (source) is excluded: there is no filesystem to read scripts from",
    ),
    (
        "trap",
        "`trap` is excluded: this shell has no signals or job control",
    ),
    (
        "wait",
        "`wait` is excluded: this shell has no job control, so nothing can be waiting",
    ),
    ("jobs", "`jobs` is excluded: this shell has no job control"),
    ("fg", "`fg` is excluded: this shell has no job control"),
    ("bg", "`bg` is excluded: this shell has no job control"),
    (
        "kill",
        "`kill` is excluded: this shell has no processes or signals",
    ),
    (
        "declare",
        "`declare` is excluded: arrays and maps are real JSON values here, so `declare -A` has nothing to declare",
    ),
    (
        "export",
        "`export` is excluded: there is no process environment to export into",
    ),
];

const RESERVED_WORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "in", "do", "done", "while", "case", "esac",
    "until", "select", "function",
];

/// A parse failure. These map to exit code `2`.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ParseError {
    /// Tokenization failed.
    #[error(transparent)]
    Lex(#[from] LexError),
    /// A syntax rule was violated.
    #[error("line {line}: {message}")]
    Syntax {
        /// One-based source line.
        line: usize,
        /// Human-readable detail.
        message: String,
    },
}

impl ParseError {
    fn syntax(line: usize, message: impl Into<String>) -> Self {
        Self::Syntax {
            line,
            message: message.into(),
        }
    }
}

/// Parses one complete script.
pub fn parse(source: &str) -> Result<Program, ParseError> {
    let tokens = tokenize(source)?;
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program(&[])?;
    if let Some(token) = parser.peek() {
        let line = token.line;
        let kind = token.kind.clone();
        return Err(ParseError::syntax(line, format!("unexpected {kind}")));
    }
    Ok(program)
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            position: 0,
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.peek().map(|token| &token.kind)
    }

    fn line(&self) -> usize {
        self.peek().map_or_else(
            || self.tokens.last().map_or(1, |token| token.line),
            |token| token.line,
        )
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek_kind(), Some(TokenKind::Newline)) {
            self.position += 1;
        }
    }

    fn skip_separators(&mut self) {
        while matches!(
            self.peek_kind(),
            Some(TokenKind::Newline | TokenKind::Semicolon)
        ) {
            self.position += 1;
        }
    }

    fn peek_reserved(&self) -> Option<&str> {
        match self.peek_kind()? {
            TokenKind::Word(word) => {
                let literal = word.as_literal()?;
                RESERVED_WORDS
                    .iter()
                    .find(|reserved| **reserved == literal)
                    .copied()
            }
            _ => None,
        }
    }

    fn eat_reserved(&mut self, expected: &str) -> bool {
        if self.peek_reserved() == Some(expected) {
            self.position += 1;
            return true;
        }
        false
    }

    fn expect_reserved(&mut self, expected: &str, context: &str) -> Result<(), ParseError> {
        if self.eat_reserved(expected) {
            return Ok(());
        }
        let line = self.line();
        Err(ParseError::syntax(
            line,
            format!("expected `{expected}` in {context}"),
        ))
    }

    fn parse_program(&mut self, terminators: &[&str]) -> Result<Program, ParseError> {
        let mut statements = Vec::new();
        loop {
            self.skip_separators();
            match self.peek_kind() {
                None | Some(TokenKind::RightBrace) => break,
                Some(_) => {}
            }
            if self
                .peek_reserved()
                .is_some_and(|reserved| terminators.contains(&reserved))
            {
                break;
            }
            statements.push(self.parse_statement()?);
            match self.peek_kind() {
                None | Some(TokenKind::Newline | TokenKind::Semicolon | TokenKind::RightBrace) => {}
                Some(TokenKind::Word(_)) if self.peek_reserved().is_some() => {}
                Some(other) => {
                    let line = self.line();
                    let other = other.clone();
                    return Err(ParseError::syntax(
                        line,
                        format!("unexpected {other} after a command"),
                    ));
                }
            }
        }
        Ok(Program { statements })
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        match self.peek_reserved() {
            Some("if") => return self.parse_if().map(Statement::If),
            Some("for") => return self.parse_for().map(Statement::For),
            Some("while") => return self.parse_while(false).map(Statement::While),
            Some("until") => return self.parse_while(true).map(Statement::While),
            // `case`/`esac` is dropped: it is pattern matching over text, and this value model
            // prefers `if` plus `jq`, which stays JSON-native.
            Some(dropped @ ("case" | "esac")) => {
                let line = self.line();
                return Err(ParseError::syntax(
                    line,
                    format!("`{dropped}` is not part of this shell; use `if`/`elif` or `jq`"),
                ));
            }
            Some("select") => {
                let line = self.line();
                return Err(ParseError::syntax(
                    line,
                    "`select` is not part of this shell: there is no interactive terminal to prompt",
                ));
            }
            Some("function") => {
                let line = self.line();
                return Err(ParseError::syntax(
                    line,
                    "the `function` keyword is not part of this shell; define functions as `name() { ... }`",
                ));
            }
            Some(other) => {
                let line = self.line();
                return Err(ParseError::syntax(line, format!("unexpected `{other}`")));
            }
            None => {}
        }

        if let Some(definition) = self.try_parse_function()? {
            return Ok(Statement::Function(definition));
        }

        self.parse_and_or_list().map(Statement::List)
    }

    fn try_parse_function(&mut self) -> Result<Option<FunctionDefinition>, ParseError> {
        let Some(TokenKind::Word(word)) = self.peek_kind() else {
            return Ok(None);
        };
        let Some(name) = word.as_literal().map(str::to_owned) else {
            return Ok(None);
        };
        if !matches!(
            self.tokens.get(self.position + 1).map(|token| &token.kind),
            Some(TokenKind::LeftParen)
        ) || !matches!(
            self.tokens.get(self.position + 2).map(|token| &token.kind),
            Some(TokenKind::RightParen)
        ) {
            return Ok(None);
        }

        let line = self.line();
        if !is_valid_name(&name) {
            return Err(ParseError::syntax(
                line,
                format!("{name:?} is not a valid function name"),
            ));
        }
        if let Some((_, reason)) = REJECTED_COMMANDS
            .iter()
            .find(|(rejected, _)| *rejected == name)
        {
            return Err(ParseError::syntax(
                line,
                format!("cannot define a function named {name:?}: {reason}"),
            ));
        }
        if RESERVED_WORDS.contains(&name.as_str()) {
            return Err(ParseError::syntax(
                line,
                format!("cannot define a function named {name:?}: it is a reserved word"),
            ));
        }

        self.position += 3;
        self.skip_newlines();
        if !matches!(self.peek_kind(), Some(TokenKind::LeftBrace)) {
            let line = self.line();
            return Err(ParseError::syntax(
                line,
                format!("expected `{{` to open the body of function {name:?}"),
            ));
        }
        self.position += 1;
        let body = self.parse_program(&[])?;
        if !matches!(self.peek_kind(), Some(TokenKind::RightBrace)) {
            let line = self.line();
            return Err(ParseError::syntax(
                line,
                format!("expected `}}` to close the body of function {name:?}"),
            ));
        }
        self.position += 1;
        Ok(Some(FunctionDefinition { name, body }))
    }

    fn parse_if(&mut self) -> Result<IfStatement, ParseError> {
        self.expect_reserved("if", "an `if` statement")?;
        let mut branches = Vec::new();
        let condition = self.parse_and_or_list()?;
        self.skip_separators();
        self.expect_reserved("then", "an `if` statement")?;
        let body = self.parse_program(&["elif", "else", "fi"])?;
        branches.push((condition, body));

        let mut otherwise = None;
        loop {
            match self.peek_reserved() {
                Some("elif") => {
                    self.position += 1;
                    let condition = self.parse_and_or_list()?;
                    self.skip_separators();
                    self.expect_reserved("then", "an `elif` branch")?;
                    let body = self.parse_program(&["elif", "else", "fi"])?;
                    branches.push((condition, body));
                }
                Some("else") => {
                    self.position += 1;
                    otherwise = Some(self.parse_program(&["fi"])?);
                    break;
                }
                _ => break,
            }
        }
        self.expect_reserved("fi", "an `if` statement")?;
        Ok(IfStatement {
            branches,
            otherwise,
        })
    }

    fn parse_for(&mut self) -> Result<ForLoop, ParseError> {
        self.expect_reserved("for", "a `for` loop")?;
        let line = self.line();
        let Some(TokenKind::Word(word)) = self.peek_kind() else {
            return Err(ParseError::syntax(line, "expected a `for` loop variable"));
        };
        let Some(variable) = word.as_literal().map(str::to_owned) else {
            return Err(ParseError::syntax(
                line,
                "a `for` loop variable must be a plain name",
            ));
        };
        if !is_valid_name(&variable) {
            return Err(ParseError::syntax(
                line,
                format!("{variable:?} is not a valid `for` loop variable name"),
            ));
        }
        self.position += 1;
        self.expect_reserved("in", "a `for` loop")?;

        let mut words = Vec::new();
        while let Some(TokenKind::Word(raw)) = self.peek_kind() {
            if self.peek_reserved() == Some("do") {
                break;
            }
            let raw = raw.clone();
            self.position += 1;
            words.push(convert_word(&raw, line)?);
        }

        self.skip_separators();
        self.expect_reserved("do", "a `for` loop")?;
        let body = self.parse_program(&["done"])?;
        self.expect_reserved("done", "a `for` loop")?;
        Ok(ForLoop {
            variable,
            words,
            body,
        })
    }

    fn parse_while(&mut self, until: bool) -> Result<WhileLoop, ParseError> {
        let keyword = if until { "until" } else { "while" };
        let context = format!("an `{keyword}` loop");
        self.expect_reserved(keyword, &context)?;
        let condition = self.parse_and_or_list()?;
        self.skip_separators();
        self.expect_reserved("do", &context)?;
        let body = self.parse_program(&["done"])?;
        self.expect_reserved("done", &context)?;
        Ok(WhileLoop {
            condition,
            body,
            until,
        })
    }

    fn parse_and_or_list(&mut self) -> Result<AndOrList, ParseError> {
        let first = self.parse_pipeline()?;
        let mut rest = Vec::new();
        loop {
            let operator = match self.peek_kind() {
                Some(TokenKind::AndAnd) => AndOr::And,
                Some(TokenKind::OrOr) => AndOr::Or,
                _ => break,
            };
            self.position += 1;
            self.skip_newlines();
            rest.push((operator, self.parse_pipeline()?));
        }
        Ok(AndOrList { first, rest })
    }

    fn parse_pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut commands = vec![self.parse_simple_command()?];
        while matches!(self.peek_kind(), Some(TokenKind::Pipe)) {
            self.position += 1;
            self.skip_newlines();
            commands.push(self.parse_simple_command()?);
        }
        Ok(Pipeline { commands })
    }

    fn parse_simple_command(&mut self) -> Result<SimpleCommand, ParseError> {
        let mut assignments = Vec::new();
        let mut words = Vec::new();
        let mut redirect: Option<Redirect> = None;
        let start_line = self.line();

        loop {
            match self.peek_kind() {
                Some(TokenKind::Word(raw)) => {
                    if words.is_empty() && self.peek_reserved().is_some() {
                        break;
                    }
                    let raw = raw.clone();
                    let line = self.line();
                    self.position += 1;
                    let assignment = if words.is_empty() {
                        split_assignment(&raw)
                    } else {
                        None
                    };
                    if let Some((name, value)) = assignment {
                        assignments.push(Assignment {
                            name,
                            value: convert_word(&value, line)?,
                        });
                        continue;
                    }
                    words.push(convert_word(&raw, line)?);
                }
                Some(TokenKind::Great | TokenKind::GreatGreat) => {
                    let append = matches!(self.peek_kind(), Some(TokenKind::GreatGreat));
                    let line = self.line();
                    self.position += 1;
                    let Some(TokenKind::Word(raw)) = self.peek_kind() else {
                        return Err(ParseError::syntax(
                            line,
                            "expected an in-memory buffer name after a redirection operator",
                        ));
                    };
                    let raw = raw.clone();
                    self.position += 1;
                    if redirect.is_some() {
                        return Err(ParseError::syntax(
                            line,
                            "a command accepts at most one buffer redirection",
                        ));
                    }
                    redirect = Some(Redirect {
                        append,
                        target: convert_word(&raw, line)?,
                    });
                }
                // Job control is dropped whole. A trailing `&` must never be silently discarded:
                // a model reading its own script would otherwise believe work was backgrounded.
                Some(TokenKind::Ampersand) => {
                    let line = self.line();
                    return Err(ParseError::syntax(
                        line,
                        "backgrounding with `&` is not supported: this shell has no job control, so `&` can only mean something it cannot do",
                    ));
                }
                // Subshell forking is dropped: there is no process to fork.
                Some(TokenKind::LeftParen) => {
                    let line = self.line();
                    return Err(ParseError::syntax(
                        line,
                        "subshells `( ... )` are not supported: this shell forks no processes; use a function instead",
                    ));
                }
                Some(TokenKind::RightParen) => {
                    let line = self.line();
                    return Err(ParseError::syntax(line, "unexpected `)`"));
                }
                // Brace command groups are dropped; only `name() { ... }` uses braces.
                Some(TokenKind::LeftBrace) => {
                    let line = self.line();
                    return Err(ParseError::syntax(
                        line,
                        "brace command groups `{ ...; }` are not supported; only function bodies use braces",
                    ));
                }
                // Here-documents and process substitution are dropped: nothing produces a file.
                Some(TokenKind::LessLess) => {
                    let line = self.line();
                    return Err(ParseError::syntax(
                        line,
                        "here-documents `<<` are not supported: use a quoted string or a named buffer written with `>`",
                    ));
                }
                Some(TokenKind::LessParen) => {
                    let line = self.line();
                    return Err(ParseError::syntax(
                        line,
                        "process substitution `<( ... )` is not supported: this shell forks no processes and has no file descriptors",
                    ));
                }
                Some(TokenKind::Less) => {
                    let line = self.line();
                    return Err(ParseError::syntax(
                        line,
                        "input redirection `<` is not supported: there are no files; pipe a value or `cat` a named buffer instead",
                    ));
                }
                _ => break,
            }
        }

        if words.is_empty() && assignments.is_empty() {
            let line = self.line();
            let found = self
                .peek_kind()
                .map_or_else(|| "end of script".to_owned(), TokenKind::to_string);
            return Err(ParseError::syntax(
                line,
                format!("expected a command, found {found}"),
            ));
        }
        let _ = start_line;

        Ok(SimpleCommand {
            assignments,
            words,
            redirect,
        })
    }
}

fn is_valid_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Splits `NAME=value` into its parts when the word begins with a valid assignment prefix.
fn split_assignment(word: &RawWord) -> Option<(String, RawWord)> {
    let RawPart::Literal(first) = word.parts.first()? else {
        return None;
    };
    let equals = first.find('=')?;
    let name = &first[..equals];
    if !is_valid_name(name) {
        return None;
    }
    let remainder = &first[equals + 1..];
    let mut parts = Vec::new();
    if !remainder.is_empty() {
        parts.push(RawPart::Literal(remainder.to_owned()));
    }
    parts.extend(word.parts.iter().skip(1).cloned());
    Some((name.to_owned(), RawWord { parts }))
}

fn convert_word(raw: &RawWord, line: usize) -> Result<Word, ParseError> {
    Ok(Word {
        parts: convert_parts(&raw.parts, line)?,
    })
}

fn convert_parts(raw: &[RawPart], line: usize) -> Result<Vec<WordPart>, ParseError> {
    raw.iter()
        .map(|part| convert_part(part, line))
        .collect::<Result<Vec<_>, _>>()
}

fn convert_part(raw: &RawPart, line: usize) -> Result<WordPart, ParseError> {
    Ok(match raw {
        RawPart::Literal(text) => WordPart::Literal(text.clone()),
        RawPart::SingleQuoted(text) => WordPart::SingleQuoted(text.clone()),
        RawPart::DoubleQuoted(parts) => WordPart::DoubleQuoted(convert_parts(parts, line)?),
        RawPart::Parameter(parameter) => WordPart::Parameter(convert_parameter(parameter, line)?),
        RawPart::CommandSubstitution(body) => WordPart::CommandSubstitution(parse(body)?),
        RawPart::Arithmetic(body) => WordPart::Arithmetic(parse_arithmetic(body, line)?),
    })
}

fn convert_parameter(raw: &RawParameter, line: usize) -> Result<Parameter, ParseError> {
    Ok(match raw {
        RawParameter::Named { name, indices } => Parameter::Named {
            name: name.clone(),
            indices: indices
                .iter()
                .map(|index| convert_word(index, line))
                .collect::<Result<Vec<_>, _>>()?,
        },
        RawParameter::Positional(position) => Parameter::Positional(*position),
        RawParameter::AllPositional => Parameter::AllPositional,
        RawParameter::PositionalCount => Parameter::PositionalCount,
        RawParameter::LastStatus => Parameter::LastStatus,
    })
}

// ---------------------------------------------------------------------------
// Arithmetic expansion
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum ArithToken {
    Integer(i64),
    Float(f64),
    Name(String),
    Symbol(&'static str),
}

fn tokenize_arithmetic(source: &str, line: usize) -> Result<Vec<ArithToken>, ParseError> {
    const SYMBOLS: &[&str] = &[
        "&&", "||", "<=", ">=", "==", "!=", "+", "-", "*", "/", "%", "(", ")", "<", ">", "!",
    ];

    let mut tokens = Vec::new();
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let character = bytes[index] as char;
        if character.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if character.is_ascii_digit() {
            let start = index;
            while index < bytes.len() && (bytes[index] as char).is_ascii_digit() {
                index += 1;
            }
            if index < bytes.len() && bytes[index] == b'.' {
                index += 1;
                while index < bytes.len() && (bytes[index] as char).is_ascii_digit() {
                    index += 1;
                }
                let literal = &source[start..index];
                let value = literal.parse::<f64>().map_err(|_| {
                    ParseError::syntax(line, format!("invalid arithmetic literal {literal:?}"))
                })?;
                tokens.push(ArithToken::Float(value));
            } else {
                let literal = &source[start..index];
                let value = literal.parse::<i64>().map_err(|_| {
                    ParseError::syntax(
                        line,
                        format!("arithmetic literal {literal:?} is out of range"),
                    )
                })?;
                tokens.push(ArithToken::Integer(value));
            }
            continue;
        }
        if character.is_ascii_alphabetic() || character == '_' || character == '$' {
            if character == '$' {
                index += 1;
            }
            let start = index;
            while index < bytes.len()
                && ((bytes[index] as char).is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            if start == index {
                return Err(ParseError::syntax(
                    line,
                    "expected a variable name after `$` in an arithmetic expansion",
                ));
            }
            tokens.push(ArithToken::Name(source[start..index].to_owned()));
            continue;
        }
        let matched = SYMBOLS
            .iter()
            .find(|symbol| source[index..].starts_with(**symbol))
            .ok_or_else(|| {
                ParseError::syntax(
                    line,
                    format!("unsupported character {character:?} in an arithmetic expansion"),
                )
            })?;
        index += matched.len();
        tokens.push(ArithToken::Symbol(matched));
    }
    Ok(tokens)
}

struct ArithParser {
    tokens: Vec<ArithToken>,
    position: usize,
    line: usize,
}

fn parse_arithmetic(source: &str, line: usize) -> Result<ArithExpr, ParseError> {
    let tokens = tokenize_arithmetic(source, line)?;
    if tokens.is_empty() {
        return Err(ParseError::syntax(line, "empty arithmetic expansion"));
    }
    let mut parser = ArithParser {
        tokens,
        position: 0,
        line,
    };
    let expression = parser.parse_or()?;
    if parser.position != parser.tokens.len() {
        return Err(ParseError::syntax(
            line,
            "trailing tokens in an arithmetic expansion",
        ));
    }
    Ok(expression)
}

impl ArithParser {
    fn peek_symbol(&self) -> Option<&'static str> {
        match self.tokens.get(self.position) {
            Some(ArithToken::Symbol(symbol)) => Some(symbol),
            _ => None,
        }
    }

    fn eat_symbol(&mut self, symbol: &str) -> bool {
        if self.peek_symbol() == Some(symbol) {
            self.position += 1;
            return true;
        }
        false
    }

    fn parse_binary_level(
        &mut self,
        operators: &[(&str, ArithBinaryOp)],
        next: fn(&mut Self) -> Result<ArithExpr, ParseError>,
    ) -> Result<ArithExpr, ParseError> {
        let mut left = next(self)?;
        while let Some(symbol) = self.peek_symbol() {
            let Some((_, operator)) = operators.iter().find(|(text, _)| *text == symbol) else {
                break;
            };
            let operator = *operator;
            self.position += 1;
            let right = next(self)?;
            left = ArithExpr::Binary(operator, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_or(&mut self) -> Result<ArithExpr, ParseError> {
        self.parse_binary_level(&[("||", ArithBinaryOp::Or)], Self::parse_and)
    }

    fn parse_and(&mut self) -> Result<ArithExpr, ParseError> {
        self.parse_binary_level(&[("&&", ArithBinaryOp::And)], Self::parse_equality)
    }

    fn parse_equality(&mut self) -> Result<ArithExpr, ParseError> {
        self.parse_binary_level(
            &[
                ("==", ArithBinaryOp::Equal),
                ("!=", ArithBinaryOp::NotEqual),
            ],
            Self::parse_relational,
        )
    }

    fn parse_relational(&mut self) -> Result<ArithExpr, ParseError> {
        self.parse_binary_level(
            &[
                ("<=", ArithBinaryOp::LessOrEqual),
                (">=", ArithBinaryOp::GreaterOrEqual),
                ("<", ArithBinaryOp::Less),
                (">", ArithBinaryOp::Greater),
            ],
            Self::parse_additive,
        )
    }

    fn parse_additive(&mut self) -> Result<ArithExpr, ParseError> {
        self.parse_binary_level(
            &[("+", ArithBinaryOp::Add), ("-", ArithBinaryOp::Subtract)],
            Self::parse_multiplicative,
        )
    }

    fn parse_multiplicative(&mut self) -> Result<ArithExpr, ParseError> {
        self.parse_binary_level(
            &[
                ("*", ArithBinaryOp::Multiply),
                ("/", ArithBinaryOp::Divide),
                ("%", ArithBinaryOp::Remainder),
            ],
            Self::parse_unary,
        )
    }

    fn parse_unary(&mut self) -> Result<ArithExpr, ParseError> {
        if self.eat_symbol("-") {
            return Ok(ArithExpr::Unary(
                ArithUnaryOp::Negate,
                Box::new(self.parse_unary()?),
            ));
        }
        if self.eat_symbol("+") {
            return self.parse_unary();
        }
        if self.eat_symbol("!") {
            return Ok(ArithExpr::Unary(
                ArithUnaryOp::Not,
                Box::new(self.parse_unary()?),
            ));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<ArithExpr, ParseError> {
        let line = self.line;
        match self.tokens.get(self.position).cloned() {
            Some(ArithToken::Integer(value)) => {
                self.position += 1;
                Ok(ArithExpr::Integer(value))
            }
            Some(ArithToken::Float(value)) => {
                self.position += 1;
                Ok(ArithExpr::Float(value))
            }
            Some(ArithToken::Name(name)) => {
                self.position += 1;
                Ok(ArithExpr::Variable(name))
            }
            Some(ArithToken::Symbol("(")) => {
                self.position += 1;
                let inner = self.parse_or()?;
                if !self.eat_symbol(")") {
                    return Err(ParseError::syntax(
                        line,
                        "unbalanced parentheses in an arithmetic expansion",
                    ));
                }
                Ok(inner)
            }
            Some(ArithToken::Symbol(symbol)) => Err(ParseError::syntax(
                line,
                format!("unexpected `{symbol}` in an arithmetic expansion"),
            )),
            None => Err(ParseError::syntax(
                line,
                "arithmetic expansion ended unexpectedly",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::{ArithBinaryOp, ArithExpr, Statement, WordPart};

    use super::{ParseError, parse};

    fn syntax_error(source: &str) -> String {
        match parse(source).expect_err("must be rejected") {
            ParseError::Syntax { message, .. } => message,
            ParseError::Lex(error) => error.message,
        }
    }

    #[test]
    fn parses_assignments_pipelines_and_lists() {
        let program = parse("x=1\necho $x | grep 1 && echo ok || echo no").expect("valid script");
        assert_eq!(program.statements.len(), 2);
        let Statement::List(list) = &program.statements[1] else {
            panic!("expected a list");
        };
        assert_eq!(list.first.commands.len(), 2);
        assert_eq!(list.rest.len(), 2);
    }

    #[test]
    fn parses_control_flow_and_functions() {
        let program = parse(
            "greet() { echo \"hi $1\"; }\nfor name in a b; do greet $name; done\nwhile false; do break; done\nif true; then echo y; elif false; then echo m; else echo n; fi",
        )
        .expect("valid script");
        assert_eq!(program.statements.len(), 4);
        assert!(matches!(program.statements[0], Statement::Function(_)));
        assert!(matches!(program.statements[1], Statement::For(_)));
        assert!(matches!(program.statements[2], Statement::While(_)));
        assert!(matches!(program.statements[3], Statement::If(_)));
    }

    #[test]
    fn parses_buffer_redirections() {
        let program = parse("echo hi > buf\necho there >> buf").expect("valid script");
        let Statement::List(list) = &program.statements[0] else {
            panic!("expected a list");
        };
        let redirect = list.first.commands[0]
            .redirect
            .as_ref()
            .expect("a redirect");
        assert!(!redirect.append);
        let Statement::List(list) = &program.statements[1] else {
            panic!("expected a list");
        };
        assert!(
            list.first.commands[0]
                .redirect
                .as_ref()
                .expect("a redirect")
                .append
        );
    }

    #[test]
    fn parses_arithmetic_with_precedence() {
        let program = parse("echo $(( 1 + 2 * 3 ))").expect("valid script");
        let Statement::List(list) = &program.statements[0] else {
            panic!("expected a list");
        };
        let WordPart::Arithmetic(expression) = &list.first.commands[0].words[1].parts[0] else {
            panic!("expected arithmetic");
        };
        let ArithExpr::Binary(ArithBinaryOp::Add, left, right) = expression else {
            panic!("expected addition at the root, found {expression:?}");
        };
        assert_eq!(**left, ArithExpr::Integer(1));
        assert!(matches!(
            **right,
            ArithExpr::Binary(ArithBinaryOp::Multiply, _, _)
        ));
    }

    #[test]
    fn backgrounding_is_a_hard_parse_error() {
        let message = syntax_error("sleep 1 &");
        assert!(message.contains("backgrounding"), "{message}");
        assert!(message.contains("job control"), "{message}");
    }

    #[test]
    fn dropped_grammar_is_rejected_by_name() {
        assert!(syntax_error("(echo hi)").contains("subshells"));
        assert!(syntax_error("{ echo hi; }").contains("brace command groups"));
        assert!(syntax_error("cat <<EOF").contains("here-documents"));
        assert!(syntax_error("diff <(a) b").contains("process substitution"));
        assert!(syntax_error("cat < file").contains("input redirection"));
        assert!(syntax_error("case $x in esac").contains("case"));
        assert!(syntax_error("select x in a; do echo $x; done").contains("select"));
        assert!(syntax_error("function f { echo hi; }").contains("`function` keyword"));
    }

    #[test]
    fn parses_until_loops_and_nested_control_flow() {
        let program = parse(
            "outer() {\n  for a in 1 2; do\n    until false; do\n      if true; then break; fi\n    done\n  done\n}\nouter",
        )
        .expect("nested control flow composes");
        assert_eq!(program.statements.len(), 2);
        let Statement::Function(definition) = &program.statements[0] else {
            panic!("expected a function definition");
        };
        let Statement::For(loop_statement) = &definition.body.statements[0] else {
            panic!("expected a for loop");
        };
        let Statement::While(inner) = &loop_statement.body.statements[0] else {
            panic!("expected an until loop");
        };
        assert!(inner.until);
        assert!(matches!(inner.body.statements[0], Statement::If(_)));
    }

    #[test]
    fn functions_cannot_shadow_rejected_commands() {
        let message = syntax_error("eval() { echo hi; }");
        assert!(
            message.contains("cannot define a function named"),
            "{message}"
        );
    }

    #[test]
    fn globbing_characters_parse_as_literal_words() {
        let program = parse("echo *").expect("an unquoted `*` is an ordinary character");
        let Statement::List(list) = &program.statements[0] else {
            panic!("expected a list");
        };
        assert_eq!(
            list.first.commands[0].words[1].parts,
            vec![WordPart::Literal("*".to_owned())]
        );
    }

    #[test]
    fn command_substitution_is_parsed_recursively() {
        let program = parse("x=$(echo hi)").expect("valid script");
        let Statement::List(list) = &program.statements[0] else {
            panic!("expected a list");
        };
        let assignment = &list.first.commands[0].assignments[0];
        assert_eq!(assignment.name, "x");
        assert!(assignment.value.is_bare_command_substitution());
    }

    #[test]
    fn unterminated_blocks_are_reported() {
        assert!(syntax_error("if true; then echo hi").contains("expected `fi`"));
        assert!(syntax_error("for x in a; do echo $x").contains("expected `done`"));
        assert!(syntax_error("f() { echo hi").contains("expected `}`"));
    }
}
