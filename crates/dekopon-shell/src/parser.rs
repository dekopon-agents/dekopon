use std::sync::Arc;

pub(crate) mod depth;
mod nested;

use depth::Depth;
use nested::Construct;
use thiserror::Error;

use crate::{
    ast::{
        AndOr, AndOrList, ArithBinaryOp, ArithExpr, ArithUnaryOp, Assignment, Background,
        CaseClause, CasePattern, CaseStatement, Command, Conditional, ConditionalTest, ForLoop,
        FunctionDefinition, IfStatement, Pattern, Pipeline, Program, Redirect, RedirectTarget,
        SimpleCommand, Statement, Stream, WhileLoop, Word, WordPart,
    },
    lexer::{LexError, Lexer, Token, TokenKind},
};

/// Without this ceiling, deeply nested command substitution overflows the native stack and aborts
/// the process with SIGABRT, which no script error can report.
const MAX_NESTING_DEPTH: u32 = 64;

const MAX_ARITHMETIC_TOKENS: usize = 4_096;

pub(crate) const REJECTED_COMMANDS: &[(&str, &str)] = &[
    ("eval", "`eval` is not supported; write the command out"),
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
    ("trap", "`trap` is excluded: this shell has no signals"),
    (
        "fg",
        "`fg` is excluded: jobs run detached and report when they finish; use `wait %N`",
    ),
    (
        "bg",
        "`bg` is excluded: jobs run detached and report when they finish; use `wait %N`",
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

pub(crate) const RESERVED_WORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "in", "do", "done", "while", "case", "esac",
    "until", "select", "function", "[[", "]]",
];

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ParseError {
    #[error(transparent)]
    Lex(#[from] LexError),
    #[error("line {line}: {message}")]
    Syntax { line: usize, message: String },
}

impl ParseError {
    fn syntax(line: usize, message: impl Into<String>) -> Self {
        Self::Syntax {
            line,
            message: message.into(),
        }
    }
}

pub fn parse(source: &str) -> Result<Program, ParseError> {
    depth::parse(source)
}

fn parse_nested(source: &str, depth: Depth) -> Result<Program, ParseError> {
    let mut parser = Parser::new(source, depth);
    let result = parser.parse_program(&[]);
    if let Some(error) = parser.lex_error.take() {
        return Err(error.into());
    }
    let program = result?;
    if let Some(token) = parser.peek() {
        let line = token.line;
        let kind = token.kind.clone();
        return Err(ParseError::syntax(line, format!("unexpected {kind}")));
    }
    Ok(program)
}

pub(crate) fn parse_substitution(
    source: &str,
    depth: Depth,
    line: usize,
) -> Result<(Program, usize), ParseError> {
    let depth = depth.substitution(line)?;
    let mut parser = Parser::new(source, depth);
    parser.substitution = true;
    let result = parser.parse_program(&[]);
    if let Some(error) = parser.lex_error.take() {
        if let Some(close) = parser.lexer.pending_here_doc_close() {
            parse_nested(&source[..close + 1], depth)?;
        }
        return Err(error.into());
    }
    let program = result?;
    let Some(token) = parser.peek() else {
        return Err(ParseError::syntax(line, "unterminated $( ... )"));
    };
    let token_line = token.line;
    let end = token.offset + 1;
    match &token.kind {
        TokenKind::RightParen => {
            if parser.lexer.offset() != end {
                // Reparse the bounded prefix to preserve the here-document's delimiter-specific refusal.
                return parse_nested(&source[..end], depth).and(Err(ParseError::syntax(
                    token_line,
                    "unterminated here-document",
                )));
            }
            Ok((program, end))
        }
        kind @ (TokenKind::Word(_)
        | TokenKind::Pipe
        | TokenKind::Semicolon
        | TokenKind::DoubleSemicolon
        | TokenKind::Newline
        | TokenKind::AndAnd
        | TokenKind::OrOr
        | TokenKind::Ampersand
        | TokenKind::LeftParen
        | TokenKind::LeftBrace
        | TokenKind::RightBrace
        | TokenKind::Redirect { .. }
        | TokenKind::Duplicate { .. }
        | TokenKind::Less
        | TokenKind::HereDoc(_)
        | TokenKind::LessParen) => {
            Err(ParseError::syntax(token_line, format!("unexpected {kind}")))
        }
    }
}

fn check_duplication_order(redirects: &[Redirect], line: usize) -> Result<(), ParseError> {
    for (index, redirect) in redirects.iter().enumerate() {
        let RedirectTarget::Stream(copied) = redirect.target else {
            continue;
        };
        let reassigned = redirects[index + 1..].iter().any(|later| {
            matches!(later.target, RedirectTarget::Buffer { .. })
                && (later.source == copied || later.source == Stream::Both)
        });
        if reassigned {
            return Err(ParseError::syntax(
                line,
                format!(
                    "`{}>&{}` must come after the redirection of stream {}, not before it: \
                     write `> name {}>&{}` rather than `{}>&{} > name`",
                    redirect.source.descriptor(),
                    copied.descriptor(),
                    copied.descriptor(),
                    redirect.source.descriptor(),
                    copied.descriptor(),
                    redirect.source.descriptor(),
                    copied.descriptor(),
                ),
            ));
        }
    }
    Ok(())
}

fn too_deep(line: usize, construct: &str) -> ParseError {
    ParseError::syntax(
        line,
        format!(
            "{construct} nested more than {MAX_NESTING_DEPTH} levels deep; this shell parses on a fixed stack and refuses rather than risking it"
        ),
    )
}

struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    position: usize,
    depth: Depth,
    lexer: Lexer<'a>,
    lex_error: Option<LexError>,
    substitution: bool,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str, depth: Depth) -> Self {
        Self {
            source,
            tokens: Vec::new(),
            position: 0,
            depth,
            lexer: Lexer::new(source, depth),
            lex_error: None,
            substitution: false,
        }
    }

    fn token_at(&mut self, position: usize) -> Option<&Token> {
        while self.tokens.len() <= position && self.lex_error.is_none() {
            self.lexer.set_parser_depth(self.depth);
            match self.lexer.next_token() {
                Ok(Some(token)) => self.tokens.push(token),
                Ok(None) => break,
                Err(error) => self.lex_error = Some(error),
            }
        }
        self.tokens.get(position)
    }

    fn peek(&mut self) -> Option<&Token> {
        self.token_at(self.position)
    }

    fn peek_kind(&mut self) -> Option<&TokenKind> {
        self.peek().map(|token| &token.kind)
    }

    fn line(&mut self) -> usize {
        match self.peek() {
            Some(token) => token.line,
            None => self.tokens.last().map_or(1, |token| token.line),
        }
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

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn peek_reserved(&mut self) -> Option<&'static str> {
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
        self.parse_program_body(terminators)
    }

    fn parse_program_body(&mut self, terminators: &[&str]) -> Result<Program, ParseError> {
        let mut statements = Vec::new();
        loop {
            self.skip_separators();
            if self.substitution && matches!(self.peek_kind(), Some(TokenKind::RightParen)) {
                break;
            }
            match self.peek_kind() {
                None | Some(TokenKind::RightBrace | TokenKind::DoubleSemicolon) => break,
                Some(_) => {}
            }
            if self
                .peek_reserved()
                .is_some_and(|reserved| terminators.contains(&reserved))
            {
                break;
            }
            let start = self.peek().map_or(0, |token| token.offset);
            let statement = self.parse_statement()?;
            if matches!(self.peek_kind(), Some(TokenKind::Ampersand)) {
                statements.push(self.background(statement, start)?);
                continue;
            }
            statements.push(statement);
            if self.substitution && matches!(self.peek_kind(), Some(TokenKind::RightParen)) {
                break;
            }
            let reserved = self.peek_reserved().is_some();
            let line = self.line();
            match self.peek_kind() {
                None
                | Some(
                    TokenKind::Newline
                    | TokenKind::Semicolon
                    | TokenKind::RightBrace
                    | TokenKind::DoubleSemicolon,
                ) => {}
                Some(TokenKind::Word(_)) if reserved => {}
                Some(other) => {
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

    fn background(&mut self, statement: Statement, start: usize) -> Result<Statement, ParseError> {
        let line = self.line();
        if let Statement::Function(definition) = &statement {
            return Err(ParseError::syntax(
                line,
                format!(
                    "`&` cannot follow the function definition `{}() {{ ... }}`: define the function, then start it as a job with `{} &`",
                    definition.name, definition.name
                ),
            ));
        }
        let source_end = self.source.len();
        let end = self.peek().map_or(source_end, |token| token.offset);
        self.position += 1;
        let text = self.source.get(start..end).unwrap_or_default().trim();
        Ok(Statement::Background(Background {
            statement: Arc::new(statement),
            text: Arc::from(text),
        }))
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        match self.peek_reserved() {
            Some("if" | "for" | "while" | "until" | "case" | "[[") => {
                return self.parse_and_or_list().map(Statement::List);
            }
            Some("esac") => {
                let line = self.line();
                return Err(ParseError::syntax(line, "`esac` without a matching `case`"));
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
            self.token_at(self.position + 1).map(|token| &token.kind),
            Some(TokenKind::LeftParen)
        ) || !matches!(
            self.token_at(self.position + 2).map(|token| &token.kind),
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
        let body = self.nest(Construct::Function)?.parse_function_body()?;
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

    fn parse_for(&mut self) -> Result<ForLoop, ParseError> {
        self.expect_reserved("for", "a `for` loop")?;
        let line = self.line();
        if matches!(self.peek_kind(), Some(TokenKind::LeftParen)) {
            return Err(ParseError::syntax(
                line,
                "C-style `for (( ... ))` loops are not supported; use `for x in ...` over a list, or a `while` loop with `i=$(( i + 1 ))`",
            ));
        }
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
        while let Some(TokenKind::Word(word)) = self.peek_kind().cloned() {
            if self.peek_reserved() == Some("do") {
                break;
            }
            self.position += 1;
            words.push(word);
        }

        self.skip_separators();
        self.expect_reserved("do", "a `for` loop")?;
        self.nest(Construct::For)?.parse_for_body(variable, words)
    }

    fn parse_case(&mut self) -> Result<CaseStatement, ParseError> {
        self.expect_reserved("case", "a `case` statement")?;
        let line = self.line();
        let Some(TokenKind::Word(word)) = self.peek_kind() else {
            return Err(ParseError::syntax(
                line,
                "expected a word to match after `case`",
            ));
        };
        let subject = word.clone();
        self.position += 1;
        self.skip_newlines();
        self.expect_reserved("in", "a `case` statement")?;

        let mut clauses = Vec::new();
        loop {
            self.skip_separators();
            if self.peek_reserved() == Some("esac") {
                break;
            }
            if self.peek().is_none() {
                let line = self.line();
                return Err(ParseError::syntax(
                    line,
                    "expected `esac` in a `case` statement",
                ));
            }
            clauses.push(self.parse_case_clause()?);
        }
        self.expect_reserved("esac", "a `case` statement")?;
        Ok(CaseStatement { subject, clauses })
    }

    fn parse_case_clause(&mut self) -> Result<CaseClause, ParseError> {
        if matches!(self.peek_kind(), Some(TokenKind::LeftParen)) {
            self.position += 1;
        }

        let mut patterns = vec![self.parse_case_pattern()?];
        while matches!(self.peek_kind(), Some(TokenKind::Pipe)) {
            self.position += 1;
            self.skip_newlines();
            patterns.push(self.parse_case_pattern()?);
        }
        if !matches!(self.peek_kind(), Some(TokenKind::RightParen)) {
            let line = self.line();
            return Err(ParseError::syntax(
                line,
                "expected `)` to close a `case` pattern list",
            ));
        }
        self.position += 1;

        self.nest(Construct::Case)?.parse_case_body(patterns)
    }

    fn parse_case_pattern(&mut self) -> Result<CasePattern, ParseError> {
        let line = self.line();
        let Some(TokenKind::Word(word)) = self.peek_kind() else {
            let found = self
                .peek_kind()
                .map_or_else(|| "end of script".to_owned(), TokenKind::to_string);
            return Err(ParseError::syntax(
                line,
                format!("expected a `case` pattern, found {found}"),
            ));
        };
        let word = word.clone();

        self.position += 1;

        if word.as_literal() == Some("*") {
            return Ok(CasePattern::Any);
        }

        if word_is_constant(&word) {
            if let Some(character) = literal_pattern_metacharacter(&word) {
                return Err(ParseError::syntax(
                    line,
                    unsupported_case_pattern(character),
                ));
            }
            return Ok(CasePattern::Literal(word));
        }
        Ok(CasePattern::Expanded(word))
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
        let negated = self.eat_pipeline_negation();
        let mut commands = vec![self.parse_command()?];
        while matches!(self.peek_kind(), Some(TokenKind::Pipe)) {
            self.position += 1;
            self.skip_newlines();
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { commands, negated })
    }

    fn parse_command(&mut self) -> Result<Command, ParseError> {
        let compound = match self.peek_reserved() {
            Some("if") => Some(Statement::If(self.nest(Construct::If)?.parse_if()?)),
            Some("for") => Some(Statement::For(self.parse_for()?)),
            Some("while") => Some(Statement::While(
                self.nest(Construct::While)?.parse_while(false)?,
            )),
            Some("until") => Some(Statement::While(
                self.nest(Construct::Until)?.parse_while(true)?,
            )),
            Some("case") => Some(Statement::Case(self.parse_case()?)),
            _ => None,
        };
        let compound = match compound {
            Some(statement) => Some(statement),
            None if self.peek_literal_word("[[") => Some(Statement::Conditional(
                self.nest(Construct::Conditional)?.parse_conditional()?,
            )),
            None if matches!(self.peek_kind(), Some(TokenKind::LeftBrace)) => Some(
                Statement::Group(self.nest(Construct::Group)?.parse_group()?),
            ),
            None => None,
        };
        let Some(statement) = compound else {
            return Ok(Command::Simple(self.parse_simple_command()?));
        };
        let redirects = self.parse_redirects()?;
        check_duplication_order(&redirects, self.line())?;
        Ok(Command::Compound {
            statement: Box::new(statement),
            redirects,
        })
    }

    fn peek_literal_word(&mut self, expected: &str) -> bool {
        matches!(self.peek_kind(), Some(TokenKind::Word(word)) if word.as_literal() == Some(expected))
    }

    fn parse_conditional_or(&mut self) -> Result<Conditional, ParseError> {
        let first = self.parse_conditional_and()?;
        if !matches!(self.peek_kind(), Some(TokenKind::OrOr)) {
            return Ok(first);
        }
        let mut terms = vec![first];
        while matches!(self.peek_kind(), Some(TokenKind::OrOr)) {
            self.position += 1;
            terms.push(self.parse_conditional_and()?);
        }
        Ok(Conditional::Or(terms))
    }

    fn parse_conditional_and(&mut self) -> Result<Conditional, ParseError> {
        let first = self.parse_conditional_unary()?;
        if !matches!(self.peek_kind(), Some(TokenKind::AndAnd)) {
            return Ok(first);
        }
        let mut terms = vec![first];
        while matches!(self.peek_kind(), Some(TokenKind::AndAnd)) {
            self.position += 1;
            terms.push(self.parse_conditional_unary()?);
        }
        Ok(Conditional::And(terms))
    }

    fn parse_conditional_unary(&mut self) -> Result<Conditional, ParseError> {
        if self.peek_literal_word("!") {
            return self.nest(Construct::TestNot)?.parse_test_not();
        }
        if matches!(self.peek_kind(), Some(TokenKind::LeftParen)) {
            return self.nest(Construct::TestGroup)?.parse_test_group();
        }
        self.parse_conditional_test()
    }

    fn parse_conditional_test(&mut self) -> Result<Conditional, ParseError> {
        let line = self.line();
        let mut words = Vec::new();
        loop {
            match self.peek_kind() {
                Some(TokenKind::Word(word)) if word.as_literal() == Some("]]") => break,
                Some(TokenKind::Word(word)) => {
                    let word = word.clone();
                    self.position += 1;
                    words.push(word);
                }
                Some(TokenKind::Less) => {
                    self.position += 1;
                    words.push(Word {
                        parts: vec![WordPart::SingleQuoted("<".to_owned())],
                    });
                }
                Some(TokenKind::Redirect {
                    source: Stream::Stdout,
                    append: false,
                }) => {
                    self.position += 1;
                    words.push(Word {
                        parts: vec![WordPart::SingleQuoted(">".to_owned())],
                    });
                }
                _ => break,
            }
        }

        if words.is_empty() {
            let found = self
                .peek_kind()
                .map_or_else(|| "end of script".to_owned(), TokenKind::to_string);
            return Err(ParseError::syntax(
                line,
                format!("expected a condition inside `[[ ... ]]`, found {found}"),
            ));
        }
        if words.len() > 3 {
            return Err(ParseError::syntax(
                line,
                "a `[[ ... ]]` condition takes at most three operands; join conditions with `&&`,                  `||`, or parentheses",
            ));
        }

        let mut check_right_pattern = false;
        if let [_, operator, right] = words.as_slice() {
            let operator = operator.as_literal().unwrap_or_default().to_owned();
            if operator == "=~" {
                return Err(ParseError::syntax(
                    line,
                    "`=~` regex matching is not supported: every pattern in this shell is literal text; compare with `==`, or match structurally with `jq`",
                ));
            }
            if matches!(operator.as_str(), "=" | "==" | "!=") {
                if word_is_constant(right) {
                    if let Some(character) = literal_pattern_metacharacter(right) {
                        return Err(ParseError::syntax(
                            line,
                            unsupported_conditional_pattern(character),
                        ));
                    }
                } else {
                    check_right_pattern = true;
                }
            }
        }

        Ok(Conditional::Test(ConditionalTest {
            words,
            check_right_pattern,
        }))
    }

    fn parse_redirects(&mut self) -> Result<Vec<Redirect>, ParseError> {
        let mut redirects = Vec::new();
        loop {
            match self.peek_kind() {
                Some(TokenKind::Redirect { source, append }) => {
                    let (source, append) = (*source, *append);
                    let line = self.line();
                    self.position += 1;
                    let Some(TokenKind::Word(word)) = self.peek_kind() else {
                        return Err(ParseError::syntax(
                            line,
                            "expected an in-memory buffer name after a redirection operator",
                        ));
                    };
                    let word = word.clone();
                    self.position += 1;
                    redirects.push(Redirect {
                        source,
                        target: RedirectTarget::Buffer {
                            append,
                            target: word,
                        },
                    });
                }
                Some(TokenKind::Duplicate { source, target }) => {
                    let (source, target) = (*source, *target);
                    let line = self.line();
                    self.position += 1;
                    if source == target {
                        return Err(ParseError::syntax(
                            line,
                            format!(
                                "`{}>&{}` redirects a stream onto itself and would do nothing",
                                source.descriptor(),
                                target.descriptor()
                            ),
                        ));
                    }
                    redirects.push(Redirect {
                        source,
                        target: RedirectTarget::Stream(target),
                    });
                }
                _ => break,
            }
        }
        Ok(redirects)
    }

    fn eat_pipeline_negation(&mut self) -> bool {
        let Some(TokenKind::Word(word)) = self.peek_kind() else {
            return false;
        };
        if word.as_literal() != Some("!") {
            return false;
        }
        self.position += 1;
        true
    }

    fn parse_simple_command(&mut self) -> Result<SimpleCommand, ParseError> {
        let mut assignments = Vec::new();
        let mut words = Vec::new();
        let mut redirects: Vec<Redirect> = Vec::new();
        let mut here_doc: Option<Word> = None;
        let mut after_empty_assignment = false;

        loop {
            match self.peek_kind().cloned() {
                Some(TokenKind::Word(word)) => {
                    if words.is_empty() && self.peek_reserved().is_some() {
                        break;
                    }
                    self.position += 1;
                    let assignment = if words.is_empty() {
                        split_assignment(&word)
                    } else {
                        None
                    };
                    if let Some((name, value)) = assignment {
                        after_empty_assignment = value.parts.is_empty();
                        assignments.push(Assignment { name, value });
                        continue;
                    }
                    after_empty_assignment = false;
                    words.push(word);
                }
                Some(TokenKind::Redirect { .. } | TokenKind::Duplicate { .. }) => {
                    after_empty_assignment = false;
                    redirects.extend(self.parse_redirects()?);
                }
                Some(TokenKind::LeftParen) => {
                    let line = self.line();
                    if after_empty_assignment {
                        return Err(ParseError::syntax(
                            line,
                            "bash array literals `name=(a b c)` are not supported: arrays here are real JSON, so write `name='[\"a\",\"b\",\"c\"]'` or `name=$(... | jq ...)` and index it with `${name[0]}`",
                        ));
                    }
                    if matches!(
                        self.token_at(self.position + 1).map(|token| &token.kind),
                        Some(TokenKind::LeftParen)
                    ) {
                        return Err(ParseError::syntax(
                            line,
                            "the arithmetic command `(( ... ))` is not supported; use the arithmetic expansion `x=$(( ... ))`, or `[ ... ]` to test a value",
                        ));
                    }
                    return Err(ParseError::syntax(
                        line,
                        "subshells `( ... )` are not supported: this shell forks no processes; use a function instead",
                    ));
                }
                Some(TokenKind::RightParen) => {
                    if self.substitution {
                        break;
                    }
                    let line = self.line();
                    return Err(ParseError::syntax(line, "unexpected `)`"));
                }
                Some(TokenKind::HereDoc(word)) => {
                    let line = self.line();
                    self.position += 1;
                    if here_doc.is_some() {
                        return Err(ParseError::syntax(
                            line,
                            "a command accepts at most one here-document",
                        ));
                    }
                    after_empty_assignment = false;
                    here_doc = Some(word);
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

        check_duplication_order(&redirects, self.line())?;
        Ok(SimpleCommand {
            assignments,
            words,
            redirects,
            here_doc,
        })
    }
}

/// `]` is deliberately absent from this table: only `[` opens a character class, so `[ab]` is still
/// caught by its bracket while a lone `a]` is left alone.
const CASE_METACHARACTERS: &[char] = &['*', '?', '['];

pub(crate) fn pattern_metacharacter(text: &str) -> Option<char> {
    text.chars()
        .find(|character| CASE_METACHARACTERS.contains(character))
}

pub(crate) fn unsupported_case_pattern(character: char) -> String {
    format!(
        "`case` patterns are literal: `{character}` is not supported; quote it as `'{character}'`, list alternatives with `|`, or use `*)` for the default"
    )
}

pub(crate) fn unsupported_parameter_pattern(character: char) -> String {
    format!("`${{NAME}}` patterns are literal: `{character}` is not supported; slice with `jq`")
}

pub(crate) fn unsupported_conditional_pattern(character: char) -> String {
    format!(
        "`[[ == ]]` patterns are literal: `{character}` is not supported; quote it as `'{character}'` or use `jq`"
    )
}

pub(crate) fn expanded_pattern(character: char) -> String {
    format!(
        "this pattern expanded to text containing `{character}`, and patterns are literal; build it without `{character}` or use `jq`"
    )
}

fn word_is_constant(word: &Word) -> bool {
    fn parts_are_constant(parts: &[WordPart]) -> bool {
        parts.iter().all(|part| match part {
            WordPart::Literal(_) | WordPart::SingleQuoted(_) => true,
            WordPart::DoubleQuoted(inner) => parts_are_constant(inner),
            WordPart::Parameter(_) | WordPart::CommandSubstitution(_) | WordPart::Arithmetic(_) => {
                false
            }
        })
    }
    parts_are_constant(&word.parts)
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn literal_pattern_metacharacter(word: &Word) -> Option<char> {
    word.parts.iter().find_map(|part| match part {
        WordPart::Literal(text) => pattern_metacharacter(text),
        _ => None,
    })
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

fn split_assignment(word: &Word) -> Option<(String, Word)> {
    let WordPart::Literal(first) = word.parts.first()? else {
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
        parts.push(WordPart::Literal(remainder.to_owned()));
    }
    parts.extend(word.parts.iter().skip(1).cloned());
    Some((name.to_owned(), Word { parts }))
}

pub(crate) fn parameter_pattern(word: Word, line: usize) -> Result<Pattern, ParseError> {
    if word_is_constant(&word) {
        if let Some(character) = literal_pattern_metacharacter(&word) {
            return Err(ParseError::syntax(
                line,
                unsupported_parameter_pattern(character),
            ));
        }
        return Ok(Pattern::Literal(word));
    }
    Ok(Pattern::Expanded(word))
}

#[derive(Clone, Debug, PartialEq)]
enum ArithToken {
    Integer(i64),
    Float(f64),
    Name(String),
    Symbol(&'static str),
}

enum ArithSymbol {
    Kept,
    Rejected(&'static str),
}

/// Operator spellings, longest first so `&&` is never mistaken for a rejected bitwise `&`.
const ARITH_SYMBOLS: &[(&str, ArithSymbol)] = &[
    (
        "**",
        ArithSymbol::Rejected("`**` is not supported; multiply repeatedly, or use `jq pow`"),
    ),
    (
        "++",
        ArithSymbol::Rejected("`++` is not supported; write `i=$(( i + 1 ))`"),
    ),
    (
        "--",
        ArithSymbol::Rejected("`--` is not supported; write `i=$(( i - 1 ))`"),
    ),
    ("+=", ArithSymbol::Rejected(COMPOUND_ASSIGNMENT)),
    ("-=", ArithSymbol::Rejected(COMPOUND_ASSIGNMENT)),
    ("*=", ArithSymbol::Rejected(COMPOUND_ASSIGNMENT)),
    ("/=", ArithSymbol::Rejected(COMPOUND_ASSIGNMENT)),
    ("%=", ArithSymbol::Rejected(COMPOUND_ASSIGNMENT)),
    ("<<", ArithSymbol::Rejected(BITWISE)),
    (">>", ArithSymbol::Rejected(BITWISE)),
    ("&&", ArithSymbol::Kept),
    ("||", ArithSymbol::Kept),
    ("<=", ArithSymbol::Kept),
    (">=", ArithSymbol::Kept),
    ("==", ArithSymbol::Kept),
    ("!=", ArithSymbol::Kept),
    ("+", ArithSymbol::Kept),
    ("-", ArithSymbol::Kept),
    ("*", ArithSymbol::Kept),
    ("/", ArithSymbol::Kept),
    ("%", ArithSymbol::Kept),
    ("(", ArithSymbol::Kept),
    (")", ArithSymbol::Kept),
    ("<", ArithSymbol::Kept),
    (">", ArithSymbol::Kept),
    ("!", ArithSymbol::Kept),
    (
        "=",
        ArithSymbol::Rejected(
            "assignment inside `$(( ... ))` is not supported; assign the expansion instead, as `name=$(( ... ))`",
        ),
    ),
    ("?", ArithSymbol::Rejected(TERNARY)),
    (":", ArithSymbol::Rejected(TERNARY)),
    ("&", ArithSymbol::Rejected(BITWISE)),
    ("|", ArithSymbol::Rejected(BITWISE)),
    ("^", ArithSymbol::Rejected(BITWISE)),
    ("~", ArithSymbol::Rejected(BITWISE)),
    (
        ",",
        ArithSymbol::Rejected("the comma operator is not supported; write one expansion per value"),
    ),
];

const COMPOUND_ASSIGNMENT: &str =
    "compound assignment is not supported inside `$(( ... ))`; write `name=$(( name + 1 ))`";
const BITWISE: &str = "bitwise operators are not supported; this arithmetic is numeric only, so use `jq` for bit manipulation";
const TERNARY: &str = "the ternary `? :` is not supported; use `if`/`else`";

#[allow(
    clippy::map_err_ignore,
    reason = "both discarded values cover a literal this scanner already shape-validated as an \
              ASCII digit run with at most one interior dot: ParseFloatError is unreachable and \
              ParseIntError can only be overflow, which the message it is replaced by states"
)]
fn tokenize_arithmetic(source: &str, line: usize) -> Result<Vec<ArithToken>, ParseError> {
    let mut tokens = Vec::new();
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if tokens.len() >= MAX_ARITHMETIC_TOKENS {
            return Err(ParseError::syntax(
                line,
                format!(
                    "an arithmetic expansion may hold at most {MAX_ARITHMETIC_TOKENS} tokens; split the calculation across assignments"
                ),
            ));
        }
        let character = source[index..].chars().next().unwrap_or('\0');
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
        let (matched, symbol) = ARITH_SYMBOLS
            .iter()
            .find(|(symbol, _)| source[index..].starts_with(*symbol))
            .ok_or_else(|| {
                ParseError::syntax(
                    line,
                    format!("unsupported character {character:?} in an arithmetic expansion"),
                )
            })?;
        match symbol {
            ArithSymbol::Kept => {}
            ArithSymbol::Rejected(reason) => return Err(ParseError::syntax(line, *reason)),
        }
        index += matched.len();
        tokens.push(ArithToken::Symbol(matched));
    }
    Ok(tokens)
}

struct ArithParser {
    tokens: Vec<ArithToken>,
    position: usize,
    line: usize,
    depth: Depth,
}

pub(crate) fn parse_arithmetic(
    source: &str,
    line: usize,
    depth: Depth,
) -> Result<ArithExpr, ParseError> {
    let tokens = tokenize_arithmetic(source, line)?;
    if tokens.is_empty() {
        return Err(ParseError::syntax(line, "empty arithmetic expansion"));
    }
    let mut parser = ArithParser {
        tokens,
        position: 0,
        line,
        depth,
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

    fn arithmetic_too_deep(&self, _error: crate::ast::ArithTooDeep) -> ParseError {
        ParseError::syntax(
            self.line,
            format!(
                "an arithmetic expansion may nest at most {MAX_NESTING_DEPTH} operators deep, counting each `!`, unary `-` and operator in a chain like `a + b + c`; split the calculation across assignments"
            ),
        )
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
            left = ArithExpr::binary(operator, left, right)
                .map_err(|error| self.arithmetic_too_deep(error))?;
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
        let mut operators = Vec::new();
        loop {
            if self.eat_symbol("-") {
                operators.push(ArithUnaryOp::Negate);
            } else if self.eat_symbol("!") {
                operators.push(ArithUnaryOp::Not);
            } else if !self.eat_symbol("+") {
                break;
            }
        }
        let mut operand = self.parse_primary()?;
        for operator in operators.into_iter().rev() {
            operand = ArithExpr::unary(operator, operand)
                .map_err(|error| self.arithmetic_too_deep(error))?;
        }
        Ok(operand)
    }

    fn parse_primary(&mut self) -> Result<ArithExpr, ParseError> {
        let line = self.line;
        match self.tokens.get(self.position).cloned() {
            Some(ArithToken::Integer(value)) => {
                self.position += 1;
                Ok(ArithExpr::integer(value))
            }
            Some(ArithToken::Float(value)) => {
                self.position += 1;
                Ok(ArithExpr::float(value))
            }
            Some(ArithToken::Name(name)) => {
                self.position += 1;
                Ok(ArithExpr::variable(name))
            }
            Some(ArithToken::Symbol("(")) => {
                self.position += 1;
                let inner = self.group()?.parse_or()?;
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
    use crate::ast::{
        ArithBinaryOp, ArithExpr, CasePattern, Command, Redirect, RedirectTarget, SimpleCommand,
        Statement, Stream, Word, WordPart,
    };

    use super::{ParseError, parse};

    fn syntax_error(source: &str) -> String {
        match parse(source).expect_err("must be rejected") {
            ParseError::Syntax { message, .. } => message,
            ParseError::Lex(error) => error.message,
        }
    }

    #[test]
    fn substitution_reports_unexpected_tokens_at_their_body_lines() {
        let cases = [
            ("$(echo })", "line 1: unexpected }"),
            ("$(echo a ;; )", "line 1: unexpected ;;"),
        ];
        let actual = cases.map(|(source, _)| parse(source).expect_err(source).to_string());
        let expected = cases.map(|(_, expected)| expected.to_owned());
        assert_eq!(actual, expected);
    }

    #[test]
    fn pending_substitution_close_keeps_the_here_doc_refusal() {
        let source = "echo \"$(cat <<EOF)\"\nhi\nEOF\n";
        assert_eq!(
            parse(source).expect_err(source).to_string(),
            "line 1: unterminated here-document: the script ended before a line containing exactly \"EOF\""
        );
    }

    #[test]
    fn a_here_doc_interpolation_failure_keeps_its_own_refusal() {
        let source = "echo $(cat <<EOF; case a in a) true;; esac\n`x`\nEOF\n)";
        assert_eq!(
            parse(source).expect_err(source).to_string(),
            "line 2: backtick command substitution is not supported; use `$( ... )`, which nests and quotes cleanly"
        );
    }

    #[test]
    fn completed_here_doc_keeps_a_later_backtick_refusal() {
        let source = "echo $(cat <<EOF; case a in a) true;; esac\nhi\nEOF\n`x`\n)";
        assert_eq!(
            parse(source).expect_err(source).to_string(),
            "line 4: backtick command substitution is not supported; use `$( ... )`, which nests and quotes cleanly"
        );
    }

    #[test]
    fn completed_here_doc_keeps_a_later_quote_refusal() {
        let source = "echo $(cat <<EOF; case a in a) true;; esac\nhi\nEOF\necho \"oops\n)";
        assert_eq!(
            parse(source).expect_err(source).to_string(),
            "line 4: unterminated double-quoted string"
        );
    }

    #[test]
    fn completed_empty_quoted_here_doc_keeps_a_later_backtick_refusal() {
        let source = "echo $(cat <<'EOF'; case a in a) true;; esac\nEOF\n`x`\n)";
        assert_eq!(
            parse(source).expect_err(source).to_string(),
            "line 3: backtick command substitution is not supported; use `$( ... )`, which nests and quotes cleanly"
        );
    }

    #[test]
    fn substitution_keeps_the_whole_case_program() {
        let command = simple_command_of(r#"echo "$(case ready in ready) echo yes;; esac)""#);
        assert_eq!(
            command.words[1],
            Word {
                parts: vec![WordPart::DoubleQuoted(vec![WordPart::CommandSubstitution(
                    crate::ast::Program {
                        statements: vec![Statement::List(crate::ast::AndOrList {
                            first: crate::ast::Pipeline {
                                negated: false,
                                commands: vec![Command::Compound {
                                    redirects: vec![],
                                    statement: Box::new(Statement::Case(
                                        crate::ast::CaseStatement {
                                            subject: Word {
                                                parts: vec![WordPart::Literal("ready".to_owned())]
                                            },
                                            clauses: vec![crate::ast::CaseClause {
                                                patterns: vec![CasePattern::Literal(Word {
                                                    parts: vec![WordPart::Literal(
                                                        "ready".to_owned()
                                                    )]
                                                })],
                                                body: crate::ast::Program {
                                                    statements: vec![Statement::List(
                                                        crate::ast::AndOrList {
                                                            first: crate::ast::Pipeline {
                                                                negated: false,
                                                                commands: vec![Command::Simple(
                                                                    simple_command_of("echo yes")
                                                                )],
                                                            },
                                                            rest: vec![],
                                                        }
                                                    )],
                                                },
                                            }],
                                        }
                                    )),
                                }],
                            },
                            rest: vec![],
                        })],
                    }
                )])]
            }
        );
    }

    #[test]
    fn parameter_default_keeps_quoted_delimiters() {
        for (source, part) in [
            (r#"echo ${x:-'}'}"#, WordPart::SingleQuoted("}".to_owned())),
            (
                r#"echo ${x:-"("}"#,
                WordPart::DoubleQuoted(vec![WordPart::Literal("(".to_owned())]),
            ),
        ] {
            assert_eq!(
                simple_command_of(source).words[1],
                Word {
                    parts: vec![WordPart::Parameter(crate::ast::Parameter::Named {
                        name: "x".to_owned(),
                        indices: vec![],
                        length: false,
                        modifier: crate::ast::Modifier::Default {
                            colon: true,
                            word: Word { parts: vec![part] }
                        },
                    })]
                }
            );
        }
    }

    #[test]
    fn parameter_default_keeps_whitespace_and_hash_text() {
        for (source, text) in [
            (r#"echo "${x:-  a  b  }""#, "  a  b  "),
            (r##"echo "${x:-#fff}""##, "#fff"),
        ] {
            assert_eq!(
                simple_command_of(source).words[1],
                Word {
                    parts: vec![WordPart::DoubleQuoted(vec![WordPart::Parameter(
                        crate::ast::Parameter::Named {
                            name: "x".to_owned(),
                            indices: vec![],
                            length: false,
                            modifier: crate::ast::Modifier::Default {
                                colon: true,
                                word: Word {
                                    parts: vec![WordPart::Literal(text.to_owned())]
                                }
                            },
                        }
                    )])]
                }
            );
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
    fn a_compound_command_parses_the_same_alone_as_in_a_pipeline() {
        let alone = compound_in(&parse("while true; do echo x; done").unwrap().statements[0]);
        let piped = {
            let program = parse("cat | while true; do echo x; done").expect("valid script");
            let Statement::List(list) = &program.statements[0] else {
                panic!("expected a list");
            };
            assert_eq!(list.first.commands.len(), 2);
            let Command::Compound { statement, .. } = &list.first.commands[1] else {
                panic!("expected a compound stage");
            };
            (**statement).clone()
        };
        assert_eq!(alone, piped);
    }

    #[test]
    fn parses_control_flow_and_functions() {
        let program = parse(
            "greet() { echo \"hi $1\"; }\nfor name in a b; do greet $name; done\nwhile false; do break; done\nif true; then echo y; elif false; then echo m; else echo n; fi",
        )
        .expect("valid script");
        assert_eq!(program.statements.len(), 4);
        assert!(matches!(program.statements[0], Statement::Function(_)));
        assert!(matches!(
            compound_in(&program.statements[1]),
            Statement::For(_)
        ));
        assert!(matches!(
            compound_in(&program.statements[2]),
            Statement::While(_)
        ));
        assert!(matches!(
            compound_in(&program.statements[3]),
            Statement::If(_)
        ));
    }

    fn compound_in(statement: &Statement) -> Statement {
        let Statement::List(list) = statement else {
            panic!("expected a list, found {statement:?}");
        };
        let Command::Compound { statement, .. } = &list.first.commands[0] else {
            panic!("expected a compound command");
        };
        (**statement).clone()
    }

    fn simple_command_of(source: &str) -> SimpleCommand {
        let program = parse(source).expect("valid script");
        let Statement::List(list) = &program.statements[0] else {
            panic!("expected a list");
        };
        let Command::Simple(command) = &list.first.commands[0] else {
            panic!("expected a simple command");
        };
        command.clone()
    }

    fn redirects_of(source: &str) -> Vec<Redirect> {
        simple_command_of(source).redirects
    }

    #[test]
    fn parses_buffer_redirections() {
        assert_eq!(
            redirects_of("echo hi > buf"),
            vec![Redirect {
                source: Stream::Stdout,
                target: RedirectTarget::Buffer {
                    append: false,
                    target: Word {
                        parts: vec![WordPart::Literal("buf".to_owned())]
                    }
                }
            }]
        );
        let appended = redirects_of("echo there >> buf");
        assert!(matches!(
            appended.as_slice(),
            [Redirect {
                target: RedirectTarget::Buffer { append: true, .. },
                ..
            }]
        ));
    }

    #[test]
    fn parses_each_stream_and_keeps_redirections_in_source_order() {
        let redirects = redirects_of("cmd > out 2> err");
        assert_eq!(redirects.len(), 2);
        assert_eq!(redirects[0].source, Stream::Stdout);
        assert_eq!(redirects[1].source, Stream::Stderr);

        assert_eq!(
            redirects_of("cmd > out 2>&1")[1],
            Redirect {
                source: Stream::Stderr,
                target: RedirectTarget::Stream(Stream::Stdout)
            }
        );
        assert_eq!(
            redirects_of("echo oops >&2"),
            vec![Redirect {
                source: Stream::Stdout,
                target: RedirectTarget::Stream(Stream::Stderr)
            }]
        );
        let both = redirects_of("cmd &> all");
        assert_eq!(both[0].source, Stream::Both);
    }

    #[test]
    fn redirections_that_would_do_nothing_or_mislead_are_refused() {
        for source in ["cmd >&1", "cmd 2>&2"] {
            let error = parse(source).expect_err("self-duplication is refused");
            assert!(format!("{error}").contains("onto itself"), "{source}");
        }
        let error = parse("cmd 2>&1 > buf").expect_err("reversed duplication is refused");
        assert!(format!("{error}").contains("before"), "{error}");
        assert_eq!(redirects_of("cmd > buf 2>&1").len(), 2);
    }

    #[test]
    fn parses_arithmetic_with_precedence() {
        let command = simple_command_of("echo $(( 1 + 2 * 3 ))");
        let WordPart::Arithmetic(expression) = &command.words[1].parts[0] else {
            panic!("expected arithmetic");
        };
        let crate::ast::ArithNode::Binary(ArithBinaryOp::Add, left, right) = expression.node()
        else {
            panic!("expected addition at the root, found {expression:?}");
        };
        assert_eq!(**left, ArithExpr::integer(1));
        assert!(matches!(
            right.node(),
            crate::ast::ArithNode::Binary(ArithBinaryOp::Multiply, _, _)
        ));
    }

    #[test]
    fn an_ampersand_starts_a_job_and_separates_like_a_semicolon() {
        let program = parse("sleep 1 & echo hi").expect("valid script");
        let [Statement::Background(job), Statement::List(_)] = program.statements.as_slice() else {
            panic!("expected a job then a list: {program:?}");
        };
        assert!(matches!(*job.statement, Statement::List(_)));
        assert_eq!(&*job.text, "sleep 1");
        for source in [
            "true & ;",
            "{ echo a; } &",
            "true && false &",
            "while false; do :; done &",
            "a | b &\necho c",
        ] {
            let program = parse(source).unwrap_or_else(|error| panic!("{source}: {error}"));
            assert!(
                matches!(program.statements[0], Statement::Background(_)),
                "{source}: {program:?}"
            );
        }
        parse("if true; then echo a & fi").expect("a job inside a branch");
        let program = parse("while false; do\n  :\ndone &").expect("valid script");
        let Statement::Background(job) = &program.statements[0] else {
            panic!("expected a job");
        };
        assert_eq!(&*job.text, "while false; do\n  :\ndone");
    }

    #[test]
    fn a_function_definition_cannot_be_started_as_a_job() {
        let message = syntax_error("f() { echo x; } &");
        assert!(message.contains("function definition `f()"), "{message}");
    }

    #[test]
    fn dropped_grammar_is_rejected_by_name() {
        assert!(syntax_error("(echo hi)").contains("subshells"));
        assert!(syntax_error("cat <<<\"$x\"").contains("here-string"));
        assert!(syntax_error("diff <(a) b").contains("process substitution"));
        assert!(syntax_error("cat < file").contains("input redirection"));
        assert!(syntax_error("select x in a; do echo $x; done").contains("select"));
        assert!(syntax_error("function f { echo hi; }").contains("`function` keyword"));
        assert!(syntax_error("esac").contains("without a matching `case`"));
    }

    #[test]
    fn parses_case_statements_with_alternatives_and_a_default() {
        let program =
            parse("case $x in\n  a|b) echo ab ;;\n  ready) echo go ;;\n  *) echo other ;;\nesac")
                .expect("valid script");
        let Statement::Case(statement) = &compound_in(&program.statements[0]) else {
            panic!(
                "expected a case statement, found {:?}",
                program.statements[0]
            );
        };
        assert_eq!(statement.clauses.len(), 3);
        assert_eq!(statement.clauses[0].patterns.len(), 2);
        assert!(matches!(statement.clauses[2].patterns[0], CasePattern::Any));
    }

    #[test]
    fn a_final_case_clause_may_omit_its_terminator() {
        let program = parse("case $x in a) echo a ;; *) echo b\nesac").expect("valid script");
        let Statement::Case(statement) = &compound_in(&program.statements[0]) else {
            panic!("expected a case statement");
        };
        assert_eq!(statement.clauses.len(), 2);
    }

    #[test]
    fn case_patterns_that_would_glob_are_rejected_by_name() {
        for (source, expected) in [
            ("case $f in *.json) echo j ;; esac", "`*`"),
            ("case $f in a?c) echo q ;; esac", "`?`"),
            ("case $f in [ab]) echo c ;; esac", "`[`"),
        ] {
            let message = syntax_error(source);
            assert!(message.contains(expected), "{source}: {message}");
            assert!(
                message.contains("patterns are literal"),
                "{source}: {message}"
            );
        }

        assert!(parse("case $f in '*') echo star ;; esac").is_ok());

        let program = parse("case $f in \\*) echo star ;; esac").expect("valid script");
        let Statement::Case(statement) = &compound_in(&program.statements[0]) else {
            panic!("expected a case statement");
        };
        assert!(matches!(
            statement.clauses[0].patterns[0],
            CasePattern::Literal(_)
        ));
        assert!(parse("case $f in a\\*b) echo star ;; esac").is_ok());
    }

    #[test]
    fn malformed_case_statements_are_reported() {
        assert!(syntax_error("case $x in a) echo a ;;").contains("expected `esac`"));
        assert!(syntax_error("case $x in a echo a ;; esac").contains("expected `)`"));
        assert!(syntax_error("case in a) echo a ;; esac").contains("expected `in`"));
    }

    #[test]
    fn a_here_document_becomes_the_command_input() {
        let command = simple_command_of("jq . <<EOF\n{\"a\": 1}\nEOF\n");
        assert_eq!(command.words.len(), 2);
        let body = command.here_doc.as_ref().expect("a here-document");
        assert_eq!(body.as_literal(), Some("{\"a\": 1}"));
    }

    #[test]
    fn a_command_accepts_at_most_one_here_document() {
        assert!(syntax_error("cat <<A <<B\na\nA\nb\nB\n").contains("at most one here-document"));
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
        let Statement::For(loop_statement) = &compound_in(&definition.body.statements[0]) else {
            panic!("expected a for loop");
        };
        let Statement::While(inner) = &compound_in(&loop_statement.body.statements[0]) else {
            panic!("expected an until loop");
        };
        assert!(inner.until);
        assert!(matches!(
            compound_in(&inner.body.statements[0]),
            Statement::If(_)
        ));
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
        assert_eq!(
            simple_command_of("echo *").words[1].parts,
            vec![WordPart::Literal("*".to_owned())],
            "an unquoted `*` is an ordinary character"
        );
    }

    #[test]
    fn command_substitution_is_parsed_recursively() {
        let command = simple_command_of("x=$(echo hi)");
        let assignment = &command.assignments[0];
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

#[cfg(test)]
#[path = "nesting_tests.rs"]
mod nesting_tests;
