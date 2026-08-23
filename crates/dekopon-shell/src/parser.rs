//! Hand-written recursive-descent parser: [`crate::lexer`] tokens to [`crate::ast`].
//!
//! Every construct the sandbox drops is rejected here by name with an actionable message. Nothing
//! is silently ignored: a script that asks for backgrounding, a subshell, process substitution, a
//! here-string, or a brace group fails to parse rather than quietly doing something else. The same
//! rule reaches inside constructs that are kept: a `case` pattern that would glob-match in bash is
//! rejected by name here rather than matched literally behind the script's back.

use thiserror::Error;

use crate::{
    ast::{
        AndOr, AndOrList, ArithBinaryOp, ArithExpr, ArithUnaryOp, Assignment, CaseClause,
        CasePattern, CaseStatement, Command, Conditional, ConditionalTest, ForLoop,
        FunctionDefinition, IfStatement, Index, Modifier, Parameter, Pattern, Pipeline, Program,
        Redirect, RedirectTarget, SimpleCommand, Statement, Stream, WhileLoop, Word, WordPart,
    },
    lexer::{
        LexError, RawIndex, RawModifier, RawParameter, RawPart, RawWord, Token, TokenKind, tokenize,
    },
};

/// How deeply the grammar may nest before parsing stops.
///
/// Command substitution, `if`/`for`/`while` bodies, and parenthesized arithmetic are all
/// recursive productions, and this parser runs on the native stack before any [`crate::limits`]
/// budget exists. Without a ceiling a few kilobytes of nested `$( $( ... ) )` overflows the stack
/// and aborts the host process with `SIGABRT`, which is not a `ScriptOutcome` any caller can
/// report. The bound is fixed rather than configurable because it is a property of this parser's
/// stack usage, not of the script's resource budget; 64 is far past any hand-written nesting and
/// far short of the depth that threatens the smallest stack this runs on.
const MAX_NESTING_DEPTH: u32 = 64;

/// How many tokens one `$(( ... ))` expansion may contain.
///
/// A flat `1 + 1 + 1 + ...` chain builds a left-leaning tree one node deep per term. Nothing walks
/// it recursively at parse time, but evaluating *and dropping* it do, so the token count is what
/// bounds that depth.
const MAX_ARITHMETIC_TOKENS: usize = 4_096;

/// Command words this shell refuses to let a script define or invoke.
///
/// Each one is excluded because it is sandbox-escape-shaped, ambient-authority-shaped, or would
/// silently change the meaning of the surrounding script — not because it was left unfinished.
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

/// Words the grammar owns, which therefore cannot be a command word or a function name.
///
/// Mirrored into `dekopon_core::RESERVED_COMMAND_WORDS` and pinned in both directions by
/// [`crate::dispatch::reserved`], so a provider can never declare one of these and then find that
/// the parser consumed the word before dispatch ever saw it.
pub(crate) const RESERVED_WORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "in", "do", "done", "while", "case", "esac",
    "until", "select", "function", "[[", "]]",
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
    parse_nested(source, 0)
}

/// Parses one script body already `depth` productions deep, for `$( ... )` re-entry.
fn parse_nested(source: &str, depth: u32) -> Result<Program, ParseError> {
    let tokens = tokenize(source)?;
    let mut parser = Parser::new(tokens, depth);
    let program = parser.parse_program(&[])?;
    if let Some(token) = parser.peek() {
        let line = token.line;
        let kind = token.kind.clone();
        return Err(ParseError::syntax(line, format!("unexpected {kind}")));
    }
    Ok(program)
}

/// Refuses a duplication whose target stream is redirected after it.
///
/// `cmd > buf 2>&1` sends both streams to `buf`; `cmd 2>&1 > buf` does not, because bash copies the
/// file *description* stdout held at that moment and a later `> buf` leaves that copy pointing at
/// the terminal. This interpreter has destinations rather than descriptions, so it cannot represent
/// the difference — and the reversed spelling is precisely the one a script writes when it believes
/// it captured diagnostics that in fact went somewhere else. It is named instead.
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

/// Reports the nesting ceiling as a syntax error a script can act on.
fn too_deep(line: usize, construct: &str) -> ParseError {
    ParseError::syntax(
        line,
        format!(
            "{construct} nested more than {MAX_NESTING_DEPTH} levels deep; this shell parses on a fixed stack and refuses rather than risking it"
        ),
    )
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    /// Recursive productions entered so far, checked against [`MAX_NESTING_DEPTH`].
    depth: u32,
}

impl Parser {
    fn new(tokens: Vec<Token>, depth: u32) -> Self {
        Self {
            tokens,
            position: 0,
            depth,
        }
    }

    /// Enters one recursive production, refusing past the nesting ceiling.
    fn enter(&mut self, construct: &str) -> Result<(), ParseError> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(too_deep(self.line(), construct));
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
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
        self.enter("a command block")?;
        let program = self.parse_program_body(terminators);
        self.leave();
        program
    }

    fn parse_program_body(&mut self, terminators: &[&str]) -> Result<Program, ParseError> {
        let mut statements = Vec::new();
        loop {
            self.skip_separators();
            match self.peek_kind() {
                // `;;` ends a `case` clause, so a clause body stops here rather than trying to
                // read it as another command.
                None | Some(TokenKind::RightBrace | TokenKind::DoubleSemicolon) => break,
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
                None
                | Some(
                    TokenKind::Newline
                    | TokenKind::Semicolon
                    | TokenKind::RightBrace
                    | TokenKind::DoubleSemicolon,
                ) => {}
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
            // Compound commands are parsed as pipeline stages even at statement level, so
            // `while ...; done | wc -l` and a bare `while` reach the same production.
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
        // `for (( i=0; i<n; i++ ))` is a C-style loop, not a malformed loop variable.
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
        while let Some(TokenKind::Word(raw)) = self.peek_kind() {
            if self.peek_reserved() == Some("do") {
                break;
            }
            let raw = raw.clone();
            self.position += 1;
            let depth = self.depth;
            words.push(convert_word(&raw, line, depth)?);
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

    /// Parses `case WORD in PATTERN) LIST ;; ... esac`.
    fn parse_case(&mut self) -> Result<CaseStatement, ParseError> {
        self.expect_reserved("case", "a `case` statement")?;
        let line = self.line();
        let Some(TokenKind::Word(raw)) = self.peek_kind() else {
            return Err(ParseError::syntax(
                line,
                "expected a word to match after `case`",
            ));
        };
        let raw = raw.clone();
        let depth = self.depth;
        self.position += 1;
        let subject = convert_word(&raw, line, depth)?;
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

    /// Parses one `PATTERN|PATTERN) LIST ;;` clause.
    fn parse_case_clause(&mut self) -> Result<CaseClause, ParseError> {
        // bash accepts a decorative `(` before the first pattern; accepting it costs nothing and
        // rejecting it would blame subshells for a shape that is not one.
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

        let body = self.parse_program(&["esac"])?;
        if matches!(self.peek_kind(), Some(TokenKind::DoubleSemicolon)) {
            self.position += 1;
        } else if self.peek_reserved() != Some("esac") {
            let line = self.line();
            return Err(ParseError::syntax(
                line,
                "expected `;;` to end a `case` clause",
            ));
        }
        Ok(CaseClause { patterns, body })
    }

    /// Parses one `case` alternative, rejecting pattern syntax this shell cannot honor.
    fn parse_case_pattern(&mut self) -> Result<CasePattern, ParseError> {
        let line = self.line();
        let Some(TokenKind::Word(raw)) = self.peek_kind() else {
            let found = self
                .peek_kind()
                .map_or_else(|| "end of script".to_owned(), TokenKind::to_string);
            return Err(ParseError::syntax(
                line,
                format!("expected a `case` pattern, found {found}"),
            ));
        };
        let raw = raw.clone();
        let depth = self.depth;
        self.position += 1;

        // A bare `*` is kept because it is the default branch, not a wildcard: every subject
        // reaches it, which is exactly what a literal matcher would also conclude.
        if raw.as_literal() == Some("*") {
            return Ok(CasePattern::Any);
        }

        let word = convert_word(&raw, line, depth)?;
        if word_is_constant(&raw) {
            if let Some((character, meaning)) = literal_pattern_metacharacter(&raw) {
                return Err(ParseError::syntax(
                    line,
                    unsupported_case_pattern(character, meaning),
                ));
            }
            return Ok(CasePattern::Literal(word));
        }
        Ok(CasePattern::Expanded(word))
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
        // A leading `!` is the reserved word that inverts a pipeline's status. Dispatching it as a
        // command word instead would report "!: command not found" and silently invert every
        // `if ! cmd` branch, so it is recognized here rather than left to the builtin table.
        let negated = self.eat_pipeline_negation();
        let mut commands = vec![self.parse_command()?];
        while matches!(self.peek_kind(), Some(TokenKind::Pipe)) {
            self.position += 1;
            self.skip_newlines();
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { commands, negated })
    }

    /// Parses one pipeline stage: a compound command, or a simple one.
    ///
    /// A compound stage is what lets `cmd | while read line; do ...; done` parse at all, and it is
    /// the same production a statement uses — so the two spellings cannot drift apart.
    fn parse_command(&mut self) -> Result<Command, ParseError> {
        let compound = match self.peek_reserved() {
            Some("if") => Some(Statement::If(self.parse_if()?)),
            Some("for") => Some(Statement::For(self.parse_for()?)),
            Some("while") => Some(Statement::While(self.parse_while(false)?)),
            Some("until") => Some(Statement::While(self.parse_while(true)?)),
            Some("case") => Some(Statement::Case(self.parse_case()?)),
            _ => None,
        };
        let compound = match compound {
            Some(statement) => Some(statement),
            None if self.peek_literal_word("[[") => {
                Some(Statement::Conditional(self.parse_conditional()?))
            }
            // A `{` opens a group only where a command may start, so `a{b}` stays one literal word
            // and a function body is still parsed by `try_parse_function`.
            None if matches!(self.peek_kind(), Some(TokenKind::LeftBrace)) => {
                Some(Statement::Group(self.parse_group()?))
            }
            None => None,
        };
        let Some(statement) = compound else {
            return Ok(Command::Simple(self.parse_simple_command()?));
        };
        let redirects = self.parse_redirects()?;
        Ok(Command::Compound {
            statement: Box::new(statement),
            redirects,
        })
    }

    /// Reports whether the next token is exactly the given literal word.
    fn peek_literal_word(&self, expected: &str) -> bool {
        matches!(self.peek_kind(), Some(TokenKind::Word(word)) if word.as_literal() == Some(expected))
    }

    /// Parses `[[ ... ]]`.
    fn parse_conditional(&mut self) -> Result<Conditional, ParseError> {
        let line = self.line();
        self.position += 1;
        self.enter("a `[[ ... ]]` conditional")?;
        let expression = self.parse_conditional_or()?;
        self.leave();
        if !self.peek_literal_word("]]") {
            let found = self
                .peek_kind()
                .map_or_else(|| "end of script".to_owned(), TokenKind::to_string);
            return Err(ParseError::syntax(
                line,
                format!("expected `]]` to close a `[[ ... ]]` conditional, found {found}"),
            ));
        }
        self.position += 1;
        Ok(expression)
    }

    fn parse_conditional_or(&mut self) -> Result<Conditional, ParseError> {
        let mut left = self.parse_conditional_and()?;
        while matches!(self.peek_kind(), Some(TokenKind::OrOr)) {
            self.position += 1;
            let right = self.parse_conditional_and()?;
            left = Conditional::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_conditional_and(&mut self) -> Result<Conditional, ParseError> {
        let mut left = self.parse_conditional_unary()?;
        while matches!(self.peek_kind(), Some(TokenKind::AndAnd)) {
            self.position += 1;
            let right = self.parse_conditional_unary()?;
            left = Conditional::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_conditional_unary(&mut self) -> Result<Conditional, ParseError> {
        if self.peek_literal_word("!") {
            self.position += 1;
            self.enter("a `!` inside `[[ ... ]]`")?;
            let inner = self.parse_conditional_unary()?;
            self.leave();
            return Ok(Conditional::Not(Box::new(inner)));
        }
        if matches!(self.peek_kind(), Some(TokenKind::LeftParen)) {
            let line = self.line();
            self.position += 1;
            self.enter("a group inside `[[ ... ]]`")?;
            let inner = self.parse_conditional_or()?;
            self.leave();
            if !matches!(self.peek_kind(), Some(TokenKind::RightParen)) {
                return Err(ParseError::syntax(
                    line,
                    "expected `)` to close a group inside `[[ ... ]]`",
                ));
            }
            self.position += 1;
            return Ok(inner);
        }
        self.parse_conditional_test()
    }

    /// Collects one primary's operands and validates its operator.
    fn parse_conditional_test(&mut self) -> Result<Conditional, ParseError> {
        let line = self.line();
        let depth = self.depth;
        let mut raws = Vec::new();
        loop {
            match self.peek_kind() {
                Some(TokenKind::Word(word)) if word.as_literal() == Some("]]") => break,
                Some(TokenKind::Word(word)) => {
                    let word = word.clone();
                    self.position += 1;
                    raws.push(word);
                }
                // Inside `[[ ]]` these are comparison operators, not redirections. The lexer has
                // no way to know that, so they are translated back into operand words here.
                Some(TokenKind::Less) => {
                    self.position += 1;
                    raws.push(RawWord {
                        parts: vec![RawPart::SingleQuoted("<".to_owned())],
                    });
                }
                Some(TokenKind::Redirect {
                    source: Stream::Stdout,
                    append: false,
                }) => {
                    self.position += 1;
                    raws.push(RawWord {
                        parts: vec![RawPart::SingleQuoted(">".to_owned())],
                    });
                }
                _ => break,
            }
        }

        if raws.is_empty() {
            let found = self
                .peek_kind()
                .map_or_else(|| "end of script".to_owned(), TokenKind::to_string);
            return Err(ParseError::syntax(
                line,
                format!("expected a condition inside `[[ ... ]]`, found {found}"),
            ));
        }
        if raws.len() > 3 {
            return Err(ParseError::syntax(
                line,
                "a `[[ ... ]]` condition takes at most three operands; join conditions with `&&`,                  `||`, or parentheses",
            ));
        }

        let mut check_right_pattern = false;
        if let [_, operator, right] = raws.as_slice() {
            let operator = operator.as_literal().unwrap_or_default().to_owned();
            if operator == "=~" {
                return Err(ParseError::syntax(
                    line,
                    "`=~` regex matching is not supported: every pattern in this shell is literal                      text; compare with `==`, or match structurally with `jq`",
                ));
            }
            if matches!(operator.as_str(), "=" | "==" | "!=") {
                // In bash this operand is a glob. Comparing it literally would answer
                // `[[ $f == *.json ]]` wrongly and silently.
                if word_is_constant(right) {
                    if let Some((character, meaning)) = literal_pattern_metacharacter(right) {
                        return Err(ParseError::syntax(
                            line,
                            unsupported_conditional_pattern(character, meaning),
                        ));
                    }
                } else {
                    check_right_pattern = true;
                }
            }
        }

        let words = raws
            .iter()
            .map(|raw| convert_word(raw, line, depth))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Conditional::Test(ConditionalTest {
            words,
            check_right_pattern,
        }))
    }

    /// Parses `{ ...; }` as a group of statements run in the current scope.
    fn parse_group(&mut self) -> Result<Program, ParseError> {
        let line = self.line();
        self.position += 1;
        self.enter("a `{ ...; }` group")?;
        let body = self.parse_program(&["}"])?;
        self.leave();
        if !matches!(self.peek_kind(), Some(TokenKind::RightBrace)) {
            return Err(ParseError::syntax(
                line,
                "expected `}` to close a `{ ...; }` group",
            ));
        }
        self.position += 1;
        if body.statements.is_empty() {
            return Err(ParseError::syntax(
                line,
                "an empty `{ }` group runs nothing; remove it or put a command in it",
            ));
        }
        Ok(body)
    }

    /// Parses the redirections trailing a compound command.
    fn parse_redirects(&mut self) -> Result<Vec<Redirect>, ParseError> {
        let mut redirects = Vec::new();
        loop {
            match self.peek_kind() {
                Some(TokenKind::Redirect { source, append }) => {
                    let (source, append) = (*source, *append);
                    let line = self.line();
                    let depth = self.depth;
                    self.position += 1;
                    let Some(TokenKind::Word(raw)) = self.peek_kind() else {
                        return Err(ParseError::syntax(
                            line,
                            "expected an in-memory buffer name after a redirection operator",
                        ));
                    };
                    let raw = raw.clone();
                    self.position += 1;
                    redirects.push(Redirect {
                        source,
                        target: RedirectTarget::Buffer {
                            append,
                            target: convert_word(&raw, line, depth)?,
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
        check_duplication_order(&redirects, self.line())?;
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
        // `arr=(a b c)` lexes as an empty assignment followed by `(`; remembering that shape is
        // what lets the paren below name array literals instead of blaming subshells.
        let mut after_empty_assignment = false;

        loop {
            match self.peek_kind() {
                Some(TokenKind::Word(raw)) => {
                    if words.is_empty() && self.peek_reserved().is_some() {
                        break;
                    }
                    let raw = raw.clone();
                    let line = self.line();
                    let depth = self.depth;
                    self.position += 1;
                    let assignment = if words.is_empty() {
                        split_assignment(&raw)
                    } else {
                        None
                    };
                    if let Some((name, value)) = assignment {
                        after_empty_assignment = value.parts.is_empty();
                        assignments.push(Assignment {
                            name,
                            value: convert_word(&value, line, depth)?,
                        });
                        continue;
                    }
                    after_empty_assignment = false;
                    words.push(convert_word(&raw, line, depth)?);
                }
                Some(TokenKind::Redirect { source, append }) => {
                    let (source, append) = (*source, *append);
                    let line = self.line();
                    let depth = self.depth;
                    self.position += 1;
                    let Some(TokenKind::Word(raw)) = self.peek_kind() else {
                        return Err(ParseError::syntax(
                            line,
                            "expected an in-memory buffer name after a redirection operator",
                        ));
                    };
                    let raw = raw.clone();
                    self.position += 1;
                    after_empty_assignment = false;
                    redirects.push(Redirect {
                        source,
                        target: RedirectTarget::Buffer {
                            append,
                            target: convert_word(&raw, line, depth)?,
                        },
                    });
                }
                Some(TokenKind::Duplicate { source, target }) => {
                    let (source, target) = (*source, *target);
                    let line = self.line();
                    self.position += 1;
                    // `>&1` and `2>&2` ask for the stream a command already writes to. Bash makes
                    // them no-ops; here they are a parse error, because a redirection that changes
                    // nothing is a script believing it moved output that never moved.
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
                    after_empty_assignment = false;
                    redirects.push(Redirect {
                        source,
                        target: RedirectTarget::Stream(target),
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
                // Every paren-shaped bash construct arrives here. They are different features with
                // different answers, so each is named for what it actually is: calling an array
                // literal a subshell sends a reader looking for a process that was never involved.
                Some(TokenKind::LeftParen) => {
                    let line = self.line();
                    if after_empty_assignment {
                        return Err(ParseError::syntax(
                            line,
                            "bash array literals `name=(a b c)` are not supported: arrays here are real JSON, so write `name='[\"a\",\"b\",\"c\"]'` or `name=$(... | jq ...)` and index it with `${name[0]}`",
                        ));
                    }
                    if matches!(
                        self.tokens.get(self.position + 1).map(|token| &token.kind),
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
                    let line = self.line();
                    return Err(ParseError::syntax(line, "unexpected `)`"));
                }
                // Brace command groups are dropped; only `name() { ... }` uses braces.
                // A here-document arrives with its body already collected off the following lines.
                Some(TokenKind::HereDoc(raw)) => {
                    let raw = raw.clone();
                    let line = self.line();
                    let depth = self.depth;
                    self.position += 1;
                    if here_doc.is_some() {
                        return Err(ParseError::syntax(
                            line,
                            "a command accepts at most one here-document",
                        ));
                    }
                    after_empty_assignment = false;
                    here_doc = Some(convert_word(&raw, line, depth)?);
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

/// Pattern syntax bash would match as a glob, and what each piece would mean there.
///
/// A `case` pattern is matched as literal text here, so silently accepting these would answer a
/// question the script never asked. The rule, and the shape of its rejection, follow `grep` and
/// `sed`, whose patterns are literal for the same reason and reject metacharacters the same way.
/// `]` is deliberately absent: only `[` opens a character class, so `[ab]` is still caught by its
/// opening bracket while a lone `a]` — ordinary text in bash too — is left alone.
const CASE_METACHARACTERS: &[(char, &str)] = &[
    ('*', "any run of characters"),
    ('?', "any single character"),
    ('[', "a character class"),
];

/// Returns the first pattern metacharacter in some text, with what it would have meant.
pub(crate) fn pattern_metacharacter(text: &str) -> Option<(char, &'static str)> {
    text.chars().find_map(|character| {
        CASE_METACHARACTERS
            .iter()
            .find(|(candidate, _)| *candidate == character)
            .map(|(candidate, meaning)| (*candidate, *meaning))
    })
}

/// Composes the rejection for a constant `case` pattern this shell cannot honor.
///
/// Quoting is offered here and *not* in [`expanded_case_pattern`], because it is only a way out
/// while the parser can still see it: by the time a pattern has been expanded, its quoting is gone.
pub(crate) fn unsupported_case_pattern(character: char, meaning: &str) -> String {
    format!(
        "a `case` pattern here is literal text, so `{character}` — which would match {meaning} in bash — is not supported; spell the value out, add another `PATTERN|PATTERN` alternative, quote it as `'{character}'` to match the character itself, or use `*)` for the default branch"
    )
}

/// Composes the rejection for a `${NAME#pattern}`-family pattern this shell cannot honor.
pub(crate) fn unsupported_parameter_pattern(character: char, meaning: &str) -> String {
    format!(
        "a `${{NAME}}` expansion pattern here is literal text, so `{character}` — which would match {meaning} in bash — is not supported; spell the text out, or slice the value with `jq` instead"
    )
}

/// Composes the rejection for a `[[ x == PATTERN ]]` operand this shell cannot honor.
pub(crate) fn unsupported_conditional_pattern(character: char, meaning: &str) -> String {
    format!(
        "the right operand of `==` inside `[[ ... ]]` is a glob in bash, and every pattern here is literal text, so `{character}` — which would match {meaning} — is not supported; quote it as `'{character}'` to compare the character itself, or match structurally with `jq`"
    )
}

/// Composes the rejection for a `[[ ]]` operand that only exists once a script has run.
pub(crate) fn expanded_conditional_pattern(character: char, meaning: &str) -> String {
    format!(
        "this `[[ ... ]]` comparison expanded to text containing `{character}`, which bash would match as {meaning}; patterns here are literal text, and quoting cannot exempt an expanded one because its quotes are already gone — compare without `{character}`, or match structurally with `jq`"
    )
}

/// Composes the rejection for a `${NAME}` expansion pattern that only exists once a script has run.
pub(crate) fn expanded_parameter_pattern(character: char, meaning: &str) -> String {
    format!(
        "this `${{NAME}}` expansion pattern expanded to text containing `{character}`, which would match {meaning} in bash; patterns here are literal text, and quoting cannot exempt an expanded one because its quotes are already gone — build the pattern without `{character}`, or slice the value with `jq` instead"
    )
}

/// Composes the rejection for a `case` pattern that only exists once the script has run.
pub(crate) fn expanded_case_pattern(character: char, meaning: &str) -> String {
    format!(
        "this `case` pattern expanded to text containing `{character}`, which would match {meaning} in bash; patterns here are literal text, and quoting cannot exempt an expanded one because its quotes are already gone — build the pattern without `{character}`, or branch with `if` and `jq` instead"
    )
}

/// Reports whether a raw word's text is fully known before the script runs.
fn word_is_constant(word: &RawWord) -> bool {
    fn parts_are_constant(parts: &[RawPart]) -> bool {
        parts.iter().all(|part| match part {
            RawPart::Literal(_) | RawPart::SingleQuoted(_) => true,
            RawPart::DoubleQuoted(inner) => parts_are_constant(inner),
            RawPart::Parameter(_) | RawPart::CommandSubstitution(_) | RawPart::Arithmetic(_) => {
                false
            }
        })
    }
    parts_are_constant(&word.parts)
}

/// Returns the first pattern metacharacter in a constant word's *unquoted* text.
///
/// Quoted text is exempt because quoting is how bash itself spells "this asterisk is an asterisk",
/// so `'*'` stays available as the way to match a literal one.
fn literal_pattern_metacharacter(word: &RawWord) -> Option<(char, &'static str)> {
    word.parts.iter().find_map(|part| match part {
        RawPart::Literal(text) => pattern_metacharacter(text),
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

fn convert_word(raw: &RawWord, line: usize, depth: u32) -> Result<Word, ParseError> {
    Ok(Word {
        parts: convert_parts(&raw.parts, line, depth)?,
    })
}

fn convert_parts(raw: &[RawPart], line: usize, depth: u32) -> Result<Vec<WordPart>, ParseError> {
    raw.iter()
        .map(|part| convert_part(part, line, depth))
        .collect::<Result<Vec<_>, _>>()
}

fn convert_part(raw: &RawPart, line: usize, depth: u32) -> Result<WordPart, ParseError> {
    Ok(match raw {
        RawPart::Literal(text) => WordPart::Literal(text.clone()),
        RawPart::SingleQuoted(text) => WordPart::SingleQuoted(text.clone()),
        RawPart::DoubleQuoted(parts) => WordPart::DoubleQuoted(convert_parts(parts, line, depth)?),
        RawPart::Parameter(parameter) => {
            WordPart::Parameter(convert_parameter(parameter, line, depth)?)
        }
        // Each `$( ... )` re-enters the parser, so it counts against the same nesting ceiling the
        // statement productions do.
        RawPart::CommandSubstitution(body) => {
            if depth >= MAX_NESTING_DEPTH {
                return Err(too_deep(line, "command substitution `$( ... )`"));
            }
            WordPart::CommandSubstitution(parse_nested(body, depth + 1)?)
        }
        RawPart::Arithmetic(body) => WordPart::Arithmetic(parse_arithmetic(body, line, depth)?),
    })
}

fn convert_parameter(raw: &RawParameter, line: usize, depth: u32) -> Result<Parameter, ParseError> {
    Ok(match raw {
        RawParameter::Named {
            name,
            indices,
            modifier,
            length,
        } => Parameter::Named {
            name: name.clone(),
            indices: indices
                .iter()
                .map(|index| convert_index(index, line, depth))
                .collect::<Result<Vec<_>, _>>()?,
            modifier: convert_modifier(modifier, line, depth)?,
            length: *length,
        },
        RawParameter::Positional(position) => Parameter::Positional(*position),
        RawParameter::AllPositional => Parameter::AllPositional,
        RawParameter::AllPositionalJoined => Parameter::AllPositionalJoined,
        RawParameter::PositionalCount => Parameter::PositionalCount,
        RawParameter::LastStatus => Parameter::LastStatus,
    })
}

/// Converts one `${NAME#pattern}`-family pattern, checking it here when it is constant.
///
/// Quoting is the escape hatch, and it only works while the parser can still see it: `${p#'*'}`
/// strips a literal asterisk, while `${p#*}` names the metacharacter and what it would have meant.
fn convert_pattern(raw: &RawWord, line: usize, depth: u32) -> Result<Pattern, ParseError> {
    let word = convert_word(raw, line, depth)?;
    if word_is_constant(raw) {
        if let Some((character, meaning)) = literal_pattern_metacharacter(raw) {
            return Err(ParseError::syntax(
                line,
                unsupported_parameter_pattern(character, meaning),
            ));
        }
        return Ok(Pattern::Literal(word));
    }
    Ok(Pattern::Expanded(word))
}

fn convert_index(raw: &RawIndex, line: usize, depth: u32) -> Result<Index, ParseError> {
    Ok(match raw {
        RawIndex::At(word) => Index::At(convert_word(word, line, depth)?),
        RawIndex::All => Index::All,
        RawIndex::AllJoined => Index::AllJoined,
    })
}

fn convert_modifier(raw: &RawModifier, line: usize, depth: u32) -> Result<Modifier, ParseError> {
    Ok(match raw {
        RawModifier::None => Modifier::None,
        RawModifier::Default { colon, word } => Modifier::Default {
            colon: *colon,
            word: convert_word(word, line, depth)?,
        },
        RawModifier::Assign { colon, word } => Modifier::Assign {
            colon: *colon,
            word: convert_word(word, line, depth)?,
        },
        RawModifier::Require { colon, word } => Modifier::Require {
            colon: *colon,
            word: word
                .as_ref()
                .map(|word| convert_word(word, line, depth))
                .transpose()?,
        },
        RawModifier::Alternate { colon, word } => Modifier::Alternate {
            colon: *colon,
            word: convert_word(word, line, depth)?,
        },
        RawModifier::StripPrefix(pattern) => {
            Modifier::StripPrefix(convert_pattern(pattern, line, depth)?)
        }
        RawModifier::StripSuffix(pattern) => {
            Modifier::StripSuffix(convert_pattern(pattern, line, depth)?)
        }
        RawModifier::Replace {
            all,
            pattern,
            replacement,
        } => Modifier::Replace {
            all: *all,
            pattern: convert_pattern(pattern, line, depth)?,
            replacement: convert_word(replacement, line, depth)?,
        },
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

/// One arithmetic operator spelling and what this shell does with it.
///
/// Rejected spellings are listed alongside the kept ones so the tokenizer can name the operator a
/// script actually wrote. Consuming `**` as two multiplications and then complaining about a stray
/// `*` describes a script nobody wrote.
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
        // Decoding a whole character rather than casting one byte keeps a non-ASCII diagnostic
        // honest: `bytes[index] as char` reports 'Ã' for an 'é' the script never wrote.
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
    /// Parenthesis nesting, checked against [`MAX_NESTING_DEPTH`]; see [`ArithParser::parse_primary`].
    depth: u32,
}

fn parse_arithmetic(source: &str, line: usize, depth: u32) -> Result<ArithExpr, ParseError> {
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
            // Each `(` re-enters the top of the precedence chain, roughly eight stack frames per
            // level, so it is bounded by the same nesting ceiling the statement grammar uses.
            Some(ArithToken::Symbol("(")) => {
                if self.depth >= MAX_NESTING_DEPTH {
                    return Err(too_deep(line, "an arithmetic expansion"));
                }
                self.position += 1;
                self.depth += 1;
                let inner = self.parse_or();
                self.depth -= 1;
                let inner = inner?;
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
        // One production, so the two spellings cannot drift apart.
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

    /// Unwraps one statement into the compound command it wraps.
    ///
    /// Every compound command is a pipeline stage now, including one written on its own line, so
    /// `while ...; done` and `cmd | while ...; done` cannot drift into two different productions.
    fn compound_in(statement: &Statement) -> Statement {
        let Statement::List(list) = statement else {
            panic!("expected a list, found {statement:?}");
        };
        let Command::Compound { statement, .. } = &list.first.commands[0] else {
            panic!("expected a compound command");
        };
        (**statement).clone()
    }

    /// Returns the only simple command of the only statement of `source`.
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

    /// Returns the redirections of the only command in the only statement of `source`.
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
        // A stream redirected onto itself moves nothing.
        for source in ["cmd >&1", "cmd 2>&2"] {
            let error = parse(source).expect_err("self-duplication is refused");
            assert!(format!("{error}").contains("onto itself"), "{source}");
        }
        // `2>&1 > buf` is the classic footgun: bash copies the *description*, so stderr keeps
        // going to the terminal while stdout moves. Nothing here can represent that difference,
        // so it is named rather than silently given the other meaning.
        let error = parse("cmd 2>&1 > buf").expect_err("reversed duplication is refused");
        assert!(format!("{error}").contains("before"), "{error}");
        // The supported spelling still parses.
        assert_eq!(redirects_of("cmd > buf 2>&1").len(), 2);
    }

    #[test]
    fn parses_arithmetic_with_precedence() {
        let command = simple_command_of("echo $(( 1 + 2 * 3 ))");
        let WordPart::Arithmetic(expression) = &command.words[1].parts[0] else {
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
        // A literal matcher would answer `*.json` wrongly and silently, which is the one thing
        // this shell will not do. `grep` and `sed` reject their metacharacters for the same reason.
        for (source, expected) in [
            ("case $f in *.json) echo j ;; esac", "any run of characters"),
            ("case $f in a?c) echo q ;; esac", "any single character"),
            ("case $f in [ab]) echo c ;; esac", "a character class"),
        ] {
            let message = syntax_error(source);
            assert!(message.contains(expected), "{source}: {message}");
            assert!(message.contains("literal text"), "{source}: {message}");
        }

        // Quoting is how bash itself spells "this asterisk is an asterisk", so it stays available.
        assert!(parse("case $f in '*') echo star ;; esac").is_ok());

        // A backslash is bash's one-character quote: `\*` is the same pattern as `'*'`. It must
        // classify as a literal match, never as the bare `*)` default branch — that would
        // silently route every subject through the escaped clause.
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
