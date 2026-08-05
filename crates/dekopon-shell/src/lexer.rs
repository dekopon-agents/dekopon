//! Tokenizer for the sandboxed shell grammar.
//!
//! The scanner is a quote state machine over `char`s. It produces operator tokens and structured
//! words; nested `$( ... )` and `$(( ... ))` bodies are captured as raw source and handed back to
//! [`crate::parser`], which re-enters itself on them.
//!
//! Constructs the sandbox drops are tokenized rather than skipped so the parser can reject them
//! with an exact message. Silently discarding a trailing `&`, for example, would let a model
//! believe backgrounding happened when nothing was backgrounded.

use std::{fmt, iter::Peekable, str::CharIndices};

use thiserror::Error;

/// One lexed token with the source line it started on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    /// What was matched.
    pub kind: TokenKind,
    /// One-based source line.
    pub line: usize,
}

/// Token classes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    /// A structured word.
    Word(RawWord),
    /// `|`.
    Pipe,
    /// `;`.
    Semicolon,
    /// `;;`, which ends one `case` clause.
    DoubleSemicolon,
    /// A line break.
    Newline,
    /// `&&`.
    AndAnd,
    /// `||`.
    OrOr,
    /// `&`. Kept as a token so the parser can hard-fail on backgrounding.
    Ampersand,
    /// `(`.
    LeftParen,
    /// `)`.
    RightParen,
    /// `{` used as a reserved word.
    LeftBrace,
    /// `}` used as a reserved word.
    RightBrace,
    /// `>`.
    Great,
    /// `>>`.
    GreatGreat,
    /// `<`. Kept so the parser can explain that there are no files to read.
    Less,
    /// A `<<DELIM` here-document, with its body already collected off the following lines.
    HereDoc(RawWord),
    /// `<(`. Kept so the parser can reject process substitution by name.
    LessParen,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rendered = match self {
            Self::Word(word) => return write!(formatter, "word {}", word.describe()),
            Self::HereDoc(_) => "here-document",
            Self::Pipe => "|",
            Self::Semicolon => ";",
            Self::DoubleSemicolon => ";;",
            Self::Newline => "newline",
            Self::AndAnd => "&&",
            Self::OrOr => "||",
            Self::Ampersand => "&",
            Self::LeftParen => "(",
            Self::RightParen => ")",
            Self::LeftBrace => "{",
            Self::RightBrace => "}",
            Self::Great => ">",
            Self::GreatGreat => ">>",
            Self::Less => "<",
            Self::LessParen => "<(",
        };
        formatter.write_str(rendered)
    }
}

/// A word before command substitutions and arithmetic have been parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawWord {
    /// Parts in source order.
    pub parts: Vec<RawPart>,
}

impl RawWord {
    /// Returns the text when the word is a single unquoted literal.
    #[must_use]
    pub fn as_literal(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [RawPart::Literal(text)] => Some(text),
            _ => None,
        }
    }

    /// Renders a short description for parser diagnostics.
    #[must_use]
    pub fn describe(&self) -> String {
        self.as_literal().map_or_else(
            || "with expansions".to_owned(),
            |literal| format!("{literal:?}"),
        )
    }
}

/// One component of a raw word.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawPart {
    /// Unquoted literal text.
    Literal(String),
    /// Single-quoted text.
    SingleQuoted(String),
    /// Double-quoted parts.
    DoubleQuoted(Vec<RawPart>),
    /// A parameter reference.
    Parameter(RawParameter),
    /// Raw `$( ... )` body.
    CommandSubstitution(String),
    /// Raw `$(( ... ))` body.
    Arithmetic(String),
}

/// A parameter reference before index words are parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawParameter {
    /// `$NAME`, `${NAME}`, `${NAME[index]...}`.
    Named {
        /// Variable name.
        name: String,
        /// Zero or more index words, applied left to right.
        indices: Vec<RawWord>,
    },
    /// `$1` .. `${N}`.
    Positional(usize),
    /// `$@`.
    AllPositional,
    /// `$*`.
    AllPositionalJoined,
    /// `$#`.
    PositionalCount,
    /// `$?`.
    LastStatus,
}

/// A tokenizer failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("line {line}: {message}")]
pub struct LexError {
    /// One-based source line.
    pub line: usize,
    /// Human-readable detail.
    pub message: String,
}

/// Why backtick command substitution is refused, in both quoting contexts.
const BACKTICK_REJECTION: &str = "backtick command substitution is not supported; use `$( ... )`, which nests and quotes cleanly";

/// Why file-descriptor redirection is refused.
const FD_REDIRECTION_REJECTION: &str = "file-descriptor redirection (`2>`, `>&2`, `2>&1`) is not supported: this shell has one combined output stream, not numbered descriptors; `>` and `>>` write named in-memory buffers";

/// Why the here-string `<<<` is refused.
///
/// It is one character away from a here-document and means something else entirely, so it is named
/// rather than left to fail as a malformed delimiter.
const HERE_STRING_REJECTION: &str = "the here-string `<<<` is not supported; pipe the value instead, as in `echo \"$x\" | cmd`, or use a here-document `<<EOF ... EOF`";

/// Why bash's fall-through `case` terminators are refused.
const CASE_FALLTHROUGH_REJECTION: &str = "`;&` and `;;&` are not supported: a `case` clause here runs alone and never falls through to the next; end every clause with `;;`";

/// A `<<DELIM` whose body has not been read yet.
///
/// The body of a here-document begins on the line *after* the operator, so the token is pushed
/// where it appears and filled in when the scanner reaches that newline. `cat <<EOF | jq .` depends
/// on that: the rest of the line is ordinary shell, and only then does the body start.
struct PendingHereDoc {
    /// Terminator line, already unquoted.
    delimiter: String,
    /// `<<-`: strip leading tabs from body lines and from the terminator.
    strip_tabs: bool,
    /// Whether the body interpolates `$NAME` and `$( )`; false when the delimiter was quoted.
    expand: bool,
    /// Line the operator appeared on, for diagnostics.
    line: usize,
    /// Index in `tokens` of the placeholder to fill in.
    token: usize,
}

impl LexError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

/// Tokenizes one script.
pub fn tokenize(source: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(source).run()
}

struct Lexer<'a> {
    source: &'a str,
    chars: Peekable<CharIndices<'a>>,
    line: usize,
    tokens: Vec<Token>,
    parts: Vec<RawPart>,
    literal: String,
    word_started: bool,
    word_line: usize,
    /// Here-documents whose operator has been seen but whose body has not started yet.
    pending_here_docs: Vec<PendingHereDoc>,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            chars: source.char_indices().peekable(),
            line: 1,
            tokens: Vec::new(),
            parts: Vec::new(),
            literal: String::new(),
            word_started: false,
            word_line: 1,
            pending_here_docs: Vec::new(),
        }
    }

    fn run(mut self) -> Result<Vec<Token>, LexError> {
        while let Some((index, character)) = self.chars.next() {
            match character {
                '\n' => {
                    self.finish_word();
                    self.push(TokenKind::Newline);
                    self.line += 1;
                    // The body of every here-document opened on the line just ended starts here.
                    self.read_pending_here_doc_bodies()?;
                }
                ' ' | '\t' | '\r' => self.finish_word(),
                '#' if !self.word_started => self.skip_comment(),
                '\\' => self.read_escape()?,
                '\'' => self.read_single_quoted()?,
                '"' => self.read_double_quoted()?,
                '$' => self.read_dollar(index)?,
                // Backticks are the one dropped construct a model reaches for by reflex, so they
                // are rejected by name rather than falling through to the literal arm below. A
                // silent literal would hand back the source text as if the command had run.
                '`' => return Err(LexError::new(self.line, BACKTICK_REJECTION)),
                '|' | '&' | ';' | '<' | '>' | '(' | ')' => self.read_operator(character)?,
                '{' | '}' if self.brace_is_reserved_word() => {
                    self.finish_word();
                    self.push(if character == '{' {
                        TokenKind::LeftBrace
                    } else {
                        TokenKind::RightBrace
                    });
                }
                other => self.push_literal(other),
            }
        }
        self.finish_word();
        if let Some(pending) = self.pending_here_docs.first() {
            return Err(LexError::new(
                pending.line,
                format!(
                    "unterminated here-document: the script ended before a line containing exactly {:?}",
                    pending.delimiter
                ),
            ));
        }
        Ok(self.tokens)
    }

    fn push(&mut self, kind: TokenKind) {
        let line = self.line;
        self.tokens.push(Token { kind, line });
    }

    fn push_literal(&mut self, character: char) {
        self.begin_word();
        self.literal.push(character);
    }

    fn begin_word(&mut self) {
        if !self.word_started {
            self.word_started = true;
            self.word_line = self.line;
        }
    }

    fn flush_literal(&mut self) {
        if !self.literal.is_empty() {
            let literal = std::mem::take(&mut self.literal);
            self.parts.push(RawPart::Literal(literal));
        }
    }

    fn push_part(&mut self, part: RawPart) {
        self.begin_word();
        self.flush_literal();
        self.parts.push(part);
    }

    fn finish_word(&mut self) {
        if !self.word_started {
            return;
        }
        self.flush_literal();
        let parts = std::mem::take(&mut self.parts);
        let line = self.word_line;
        self.word_started = false;
        self.tokens.push(Token {
            kind: TokenKind::Word(RawWord { parts }),
            line,
        });
    }

    /// `{` and `}` are reserved words only as complete words, so `a{b}` stays one literal word.
    ///
    /// Brace expansion (`{a,b,c}`) is dropped: braces inside a word are ordinary characters.
    fn brace_is_reserved_word(&mut self) -> bool {
        if self.word_started {
            return false;
        }
        matches!(
            self.chars.peek().map(|(_, character)| *character),
            None | Some(' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>')
        )
    }

    fn skip_comment(&mut self) {
        while let Some((_, character)) = self.chars.peek() {
            if *character == '\n' {
                return;
            }
            self.chars.next();
        }
    }

    fn read_escape(&mut self) -> Result<(), LexError> {
        match self.chars.next() {
            Some((_, '\n')) => {
                self.line += 1;
                Ok(())
            }
            Some((_, character)) => {
                self.push_literal(character);
                Ok(())
            }
            None => Err(LexError::new(
                self.line,
                "script ends with a trailing backslash",
            )),
        }
    }

    fn read_single_quoted(&mut self) -> Result<(), LexError> {
        let opened = self.line;
        let mut text = String::new();
        loop {
            match self.chars.next() {
                Some((_, '\'')) => break,
                Some((_, character)) => {
                    if character == '\n' {
                        self.line += 1;
                    }
                    text.push(character);
                }
                None => {
                    return Err(LexError::new(opened, "unterminated single-quoted string"));
                }
            }
        }
        self.push_part(RawPart::SingleQuoted(text));
        Ok(())
    }

    fn read_double_quoted(&mut self) -> Result<(), LexError> {
        let parts = self.read_interpolated(Some('"'), "unterminated double-quoted string")?;
        self.push_part(RawPart::DoubleQuoted(parts));
        Ok(())
    }

    /// Scans interpolated text: literals, `$NAME`, `$( )`, and `$(( ))`.
    ///
    /// Shared by double-quoted strings and by the body of an unquoted here-document, because bash
    /// interpolates both by the same rules. The one difference is which characters a backslash may
    /// escape: `\"` is an escaped quote inside quotes and ordinary text inside a here-document,
    /// where collapsing it would silently corrupt embedded JSON such as `{"a": "\"x\""}`.
    fn read_interpolated(
        &mut self,
        terminator: Option<char>,
        unterminated: &str,
    ) -> Result<Vec<RawPart>, LexError> {
        let opened = self.line;
        let escapable: &[char] = if terminator == Some('"') {
            &['"', '\\', '$', '`']
        } else {
            &['\\', '$', '`']
        };
        let mut parts = Vec::new();
        let mut literal = String::new();
        loop {
            let Some((index, character)) = self.chars.next() else {
                if terminator.is_none() {
                    break;
                }
                return Err(LexError::new(opened, unterminated));
            };
            if Some(character) == terminator {
                break;
            }
            match character {
                '\\' => match self.chars.next() {
                    Some((_, escaped)) if escapable.contains(&escaped) => literal.push(escaped),
                    Some((_, '\n')) => self.line += 1,
                    Some((_, other)) => {
                        literal.push('\\');
                        literal.push(other);
                    }
                    None => {
                        if terminator.is_none() {
                            literal.push('\\');
                            break;
                        }
                        return Err(LexError::new(opened, unterminated));
                    }
                },
                '$' => {
                    let part = self.read_dollar_part(index)?;
                    match part {
                        Some(part) => {
                            if !literal.is_empty() {
                                parts.push(RawPart::Literal(std::mem::take(&mut literal)));
                            }
                            parts.push(part);
                        }
                        None => literal.push('$'),
                    }
                }
                '`' => return Err(LexError::new(self.line, BACKTICK_REJECTION)),
                other => {
                    if other == '\n' {
                        self.line += 1;
                    }
                    literal.push(other);
                }
            }
        }
        if !literal.is_empty() {
            parts.push(RawPart::Literal(literal));
        }
        Ok(parts)
    }

    /// Records a `<<DELIM` operator, leaving a placeholder token for its body.
    fn read_here_doc_header(&mut self) -> Result<(), LexError> {
        if self.chars.peek().map(|(_, character)| *character) == Some('<') {
            return Err(LexError::new(self.line, HERE_STRING_REJECTION));
        }
        let strip_tabs = self.chars.peek().map(|(_, character)| *character) == Some('-');
        if strip_tabs {
            self.chars.next();
        }
        while matches!(
            self.chars.peek().map(|(_, character)| *character),
            Some(' ' | '\t')
        ) {
            self.chars.next();
        }

        let (delimiter, expand) = self.read_here_doc_delimiter()?;
        let line = self.line;
        let token = self.tokens.len();
        self.tokens.push(Token {
            kind: TokenKind::HereDoc(RawWord { parts: Vec::new() }),
            line,
        });
        self.pending_here_docs.push(PendingHereDoc {
            delimiter,
            strip_tabs,
            expand,
            line,
            token,
        });
        Ok(())
    }

    /// Reads the terminator word after `<<`, reporting whether the body interpolates.
    ///
    /// Any quoting anywhere in the delimiter turns interpolation off, exactly as in bash: `<<'EOF'`,
    /// `<<"EOF"`, and `<<\EOF` all mean "this body is literal text".
    fn read_here_doc_delimiter(&mut self) -> Result<(String, bool), LexError> {
        let line = self.line;
        let mut delimiter = String::new();
        let mut quoted = false;
        while let Some((_, character)) = self.chars.peek().copied() {
            match character {
                '\'' | '"' => {
                    self.chars.next();
                    quoted = true;
                    loop {
                        match self.chars.next() {
                            Some((_, closing)) if closing == character => break,
                            Some((_, '\n')) | None => {
                                return Err(LexError::new(
                                    line,
                                    "unterminated quoted here-document delimiter",
                                ));
                            }
                            Some((_, other)) => delimiter.push(other),
                        }
                    }
                }
                '\\' => {
                    self.chars.next();
                    quoted = true;
                    match self.chars.next() {
                        Some((_, escaped)) => delimiter.push(escaped),
                        None => {
                            return Err(LexError::new(
                                line,
                                "script ends with a trailing backslash",
                            ));
                        }
                    }
                }
                other if other.is_ascii_alphanumeric() || matches!(other, '_' | '.' | '-') => {
                    delimiter.push(other);
                    self.chars.next();
                }
                _ => break,
            }
        }
        if delimiter.is_empty() {
            return Err(LexError::new(
                line,
                "expected a here-document delimiter after `<<`, as in `cat <<EOF`",
            ));
        }
        Ok((delimiter, !quoted))
    }

    /// Consumes the body of every here-document opened on the line that just ended.
    fn read_pending_here_doc_bodies(&mut self) -> Result<(), LexError> {
        // Several here-documents may open on one line (`cmd <<A <<B`); bash reads their bodies in
        // the order the operators appeared, and so does this.
        let pending = std::mem::take(&mut self.pending_here_docs);
        for specification in pending {
            let mut body = self.read_here_doc_body(&specification)?;
            // Drop the newline that ended the last body line. Values in this shell are not
            // newline-terminated — `echo hi` produces `"hi"`, and emitting a value adds the line
            // ending — so keeping it would make `cat <<EOF` print a trailing blank line that the
            // same here-document does not produce in bash.
            body.pop();
            let parts = if specification.expand {
                Self::interpolate_here_doc_body(&body, specification.line)?
            } else if body.is_empty() {
                Vec::new()
            } else {
                vec![RawPart::Literal(body)]
            };
            self.tokens[specification.token] = Token {
                kind: TokenKind::HereDoc(RawWord { parts }),
                line: specification.line,
            };
        }
        Ok(())
    }

    /// Reads raw body lines up to the terminator line.
    fn read_here_doc_body(&mut self, specification: &PendingHereDoc) -> Result<String, LexError> {
        let mut body = String::new();
        loop {
            let mut line = String::new();
            let mut terminated = false;
            for (_, character) in self.chars.by_ref() {
                if character == '\n' {
                    terminated = true;
                    break;
                }
                line.push(character);
            }
            if terminated {
                self.line += 1;
            }

            // `<<-` strips leading tabs — and only tabs, never spaces — from both the body lines
            // and the terminator, which is what lets a here-document sit at the indentation of the
            // block around it.
            let content = if specification.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line.as_str()
            };
            if content == specification.delimiter {
                return Ok(body);
            }
            if !terminated {
                return Err(LexError::new(
                    specification.line,
                    format!(
                        "unterminated here-document: the script ended before a line containing exactly {:?}",
                        specification.delimiter
                    ),
                ));
            }
            body.push_str(content);
            body.push('\n');
        }
    }

    /// Interpolates an unquoted here-document body by re-scanning it as quoted-style text.
    fn interpolate_here_doc_body(body: &str, line: usize) -> Result<Vec<RawPart>, LexError> {
        let mut lexer = Lexer::new(body);
        lexer.line = line;
        lexer
            .read_interpolated(None, "unterminated here-document")
            .map_err(|error| LexError::new(error.line, error.message))
    }

    fn read_dollar(&mut self, index: usize) -> Result<(), LexError> {
        match self.read_dollar_part(index)? {
            Some(part) => self.push_part(part),
            // A `$` that introduces nothing recognizable is an ordinary character, as in bash.
            None => self.push_literal('$'),
        }
        Ok(())
    }

    fn read_dollar_part(&mut self, dollar_index: usize) -> Result<Option<RawPart>, LexError> {
        let Some((_, next)) = self.chars.peek().copied() else {
            return Ok(None);
        };
        match next {
            '(' => {
                self.chars.next();
                if self.chars.peek().map(|(_, character)| *character) == Some('(') {
                    self.chars.next();
                    let body = self.read_balanced(dollar_index, '(', ')', 2, "$(( ... ))")?;
                    return Ok(Some(RawPart::Arithmetic(body)));
                }
                let body = self.read_balanced(dollar_index, '(', ')', 1, "$( ... )")?;
                Ok(Some(RawPart::CommandSubstitution(body)))
            }
            '{' => {
                self.chars.next();
                self.read_braced_parameter().map(Some)
            }
            '?' => {
                self.chars.next();
                Ok(Some(RawPart::Parameter(RawParameter::LastStatus)))
            }
            '@' => {
                self.chars.next();
                Ok(Some(RawPart::Parameter(RawParameter::AllPositional)))
            }
            '*' => {
                self.chars.next();
                Ok(Some(RawPart::Parameter(RawParameter::AllPositionalJoined)))
            }
            '#' => {
                self.chars.next();
                Ok(Some(RawPart::Parameter(RawParameter::PositionalCount)))
            }
            digit if digit.is_ascii_digit() => {
                self.chars.next();
                let position = usize::from(
                    digit
                        .to_digit(10)
                        .and_then(|value| u8::try_from(value).ok())
                        .unwrap_or_default(),
                );
                Ok(Some(RawPart::Parameter(RawParameter::Positional(position))))
            }
            first if first.is_ascii_alphabetic() || first == '_' => {
                let name = self.read_name();
                Ok(Some(RawPart::Parameter(RawParameter::Named {
                    name,
                    indices: Vec::new(),
                })))
            }
            _ => Ok(None),
        }
    }

    fn read_name(&mut self) -> String {
        let mut name = String::new();
        while let Some((_, character)) = self.chars.peek().copied() {
            if character.is_ascii_alphanumeric() || character == '_' {
                name.push(character);
                self.chars.next();
            } else {
                break;
            }
        }
        name
    }

    fn read_braced_parameter(&mut self) -> Result<RawPart, LexError> {
        let line = self.line;
        match self.chars.peek().map(|(_, character)| *character) {
            Some('?') => {
                self.chars.next();
                self.expect_brace_close(line)?;
                return Ok(RawPart::Parameter(RawParameter::LastStatus));
            }
            Some('@') => {
                self.chars.next();
                self.expect_brace_close(line)?;
                return Ok(RawPart::Parameter(RawParameter::AllPositional));
            }
            Some('*') => {
                self.chars.next();
                self.expect_brace_close(line)?;
                return Ok(RawPart::Parameter(RawParameter::AllPositionalJoined));
            }
            Some('#') => {
                self.chars.next();
                if self.chars.peek().map(|(_, character)| *character) == Some('}') {
                    self.chars.next();
                    return Ok(RawPart::Parameter(RawParameter::PositionalCount));
                }
                return Err(LexError::new(
                    line,
                    "${#name} length expansion is not supported; use `jq length` or `wc` instead",
                ));
            }
            Some(digit) if digit.is_ascii_digit() => {
                let mut digits = String::new();
                while let Some((_, character)) = self.chars.peek().copied() {
                    if character.is_ascii_digit() {
                        digits.push(character);
                        self.chars.next();
                    } else {
                        break;
                    }
                }
                self.expect_brace_close(line)?;
                let position = digits.parse::<usize>().map_err(|_| {
                    LexError::new(
                        line,
                        format!("positional parameter ${digits} is out of range"),
                    )
                })?;
                return Ok(RawPart::Parameter(RawParameter::Positional(position)));
            }
            _ => {}
        }

        let name = self.read_name();
        if name.is_empty() {
            return Err(LexError::new(line, "empty ${} parameter reference"));
        }

        let mut indices = Vec::new();
        loop {
            match self.chars.peek().map(|(_, character)| *character) {
                Some('}') => {
                    self.chars.next();
                    break;
                }
                Some('[') => {
                    self.chars.next();
                    indices.push(self.read_index_word(line)?);
                }
                Some(other) => {
                    return Err(LexError::new(
                        line,
                        format!(
                            "unsupported ${{{name}{other}...}} parameter expansion; this shell keeps only ${{NAME}} and ${{NAME[index]}}"
                        ),
                    ));
                }
                None => return Err(LexError::new(line, "unterminated ${} parameter reference")),
            }
        }

        Ok(RawPart::Parameter(RawParameter::Named { name, indices }))
    }

    fn expect_brace_close(&mut self, line: usize) -> Result<(), LexError> {
        match self.chars.next() {
            Some((_, '}')) => Ok(()),
            _ => Err(LexError::new(line, "unterminated ${} parameter reference")),
        }
    }

    /// Reads the index text inside `${NAME[...]}`.
    ///
    /// Bash's own sparse and associative array emulation is dropped: `${NAME[@]}` and `${NAME[*]}`
    /// are rejected by name because indexing here is backed by real JSON arrays and objects.
    fn read_index_word(&mut self, line: usize) -> Result<RawWord, LexError> {
        let mut text = String::new();
        loop {
            match self.chars.next() {
                Some((_, ']')) => break,
                Some((_, '\n')) => {
                    return Err(LexError::new(line, "unterminated ${NAME[index]} reference"));
                }
                Some((_, character)) => text.push(character),
                None => return Err(LexError::new(line, "unterminated ${NAME[index]} reference")),
            }
        }
        if text == "@" || text == "*" {
            return Err(LexError::new(
                line,
                "${NAME[@]} array expansion is not supported; an unquoted $NAME holding a JSON array already expands element by element",
            ));
        }
        let tokens = tokenize(&text)?;
        let mut words = tokens.into_iter().filter_map(|token| match token.kind {
            TokenKind::Word(word) => Some(word),
            _ => None,
        });
        let word = words
            .next()
            .ok_or_else(|| LexError::new(line, "empty ${NAME[index]} reference"))?;
        if words.next().is_some() {
            return Err(LexError::new(
                line,
                "${NAME[index]} accepts exactly one index expression",
            ));
        }
        Ok(word)
    }

    /// Captures a balanced `$( ... )` or `$(( ... ))` body as raw source.
    fn read_balanced(
        &mut self,
        dollar_index: usize,
        open: char,
        close: char,
        mut depth: usize,
        label: &str,
    ) -> Result<String, LexError> {
        let opened = self.line;
        let initial = depth;
        let start = dollar_index + '$'.len_utf8() + open.len_utf8() * depth;
        // `$(( ... ))` closes two levels, so the body ends at the *first* closing parenthesis while
        // scanning continues to the second. Recording that position keeps the captured body exact.
        let mut body_end = None;
        loop {
            let Some((index, character)) = self.chars.next() else {
                return Err(LexError::new(opened, format!("unterminated {label}")));
            };
            match character {
                '\n' => self.line += 1,
                '\\' => {
                    if let Some((_, next)) = self.chars.next() {
                        if next == '\n' {
                            self.line += 1;
                        }
                        continue;
                    }
                }
                '\'' => {
                    for (_, quoted) in self.chars.by_ref() {
                        if quoted == '\n' {
                            self.line += 1;
                        }
                        if quoted == '\'' {
                            break;
                        }
                    }
                    continue;
                }
                '"' => {
                    let mut escaped = false;
                    for (_, quoted) in self.chars.by_ref() {
                        if quoted == '\n' {
                            self.line += 1;
                        }
                        if escaped {
                            escaped = false;
                            continue;
                        }
                        if quoted == '\\' {
                            escaped = true;
                            continue;
                        }
                        if quoted == '"' {
                            break;
                        }
                    }
                    continue;
                }
                matched if matched == open => depth += 1,
                matched if matched == close => {
                    depth -= 1;
                    if depth + 1 == initial && body_end.is_none() {
                        body_end = Some(index);
                    }
                    if depth == 0 {
                        let end = body_end.unwrap_or(index);
                        return Ok(self.source[start..end].to_owned());
                    }
                }
                _ => {}
            }
        }
    }

    fn read_operator(&mut self, character: char) -> Result<(), LexError> {
        // `2>`, `2>&1`, and `>&2` all begin as a bare digit word glued to a redirection operator.
        // Letting the digit finish as an ordinary word would append it to argv and divert the
        // output into a buffer named `/dev/null`, so the whole shape is rejected by name instead.
        if matches!(character, '<' | '>')
            && self.word_started
            && self.parts.is_empty()
            && !self.literal.is_empty()
            && self.literal.chars().all(|digit| digit.is_ascii_digit())
        {
            return Err(LexError::new(self.line, FD_REDIRECTION_REJECTION));
        }
        self.finish_word();
        let next = self.chars.peek().map(|(_, character)| *character);
        if matches!((character, next), ('<' | '>', Some('&'))) {
            return Err(LexError::new(self.line, FD_REDIRECTION_REJECTION));
        }
        let kind = match (character, next) {
            ('|', Some('|')) => {
                self.chars.next();
                TokenKind::OrOr
            }
            ('|', _) => TokenKind::Pipe,
            ('&', Some('&')) => {
                self.chars.next();
                TokenKind::AndAnd
            }
            ('&', _) => TokenKind::Ampersand,
            (';', Some(';')) => {
                self.chars.next();
                // `;;&` and `;&` are bash's two fall-through terminators. Reading either as a
                // plain `;;` would run one clause where the script asked for several.
                if self.chars.peek().map(|(_, character)| *character) == Some('&') {
                    return Err(LexError::new(self.line, CASE_FALLTHROUGH_REJECTION));
                }
                TokenKind::DoubleSemicolon
            }
            (';', Some('&')) => return Err(LexError::new(self.line, CASE_FALLTHROUGH_REJECTION)),
            (';', _) => TokenKind::Semicolon,
            ('>', Some('>')) => {
                self.chars.next();
                TokenKind::GreatGreat
            }
            ('>', _) => TokenKind::Great,
            ('<', Some('<')) => {
                self.chars.next();
                return self.read_here_doc_header();
            }
            ('<', Some('(')) => {
                self.chars.next();
                TokenKind::LessParen
            }
            ('<', _) => TokenKind::Less,
            ('(', _) => TokenKind::LeftParen,
            (')', _) => TokenKind::RightParen,
            _ => unreachable!("read_operator is only called for operator characters"),
        };
        self.push(kind);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{RawParameter, RawPart, TokenKind, tokenize};

    fn kinds(source: &str) -> Vec<TokenKind> {
        tokenize(source)
            .expect("tokenizes")
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    fn single_word(source: &str) -> Vec<RawPart> {
        match kinds(source).into_iter().next().expect("one token") {
            TokenKind::Word(word) => word.parts,
            other => panic!("expected a word, found {other}"),
        }
    }

    #[test]
    fn splits_words_and_operators() {
        assert_eq!(
            kinds("echo hi | grep h && true"),
            vec![
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("echo".to_owned())]
                }),
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("hi".to_owned())]
                }),
                TokenKind::Pipe,
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("grep".to_owned())]
                }),
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("h".to_owned())]
                }),
                TokenKind::AndAnd,
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("true".to_owned())]
                }),
            ]
        );
    }

    #[test]
    fn comments_run_to_end_of_line() {
        assert_eq!(
            kinds("echo a # trailing\necho b"),
            vec![
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("echo".to_owned())]
                }),
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("a".to_owned())]
                }),
                TokenKind::Newline,
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("echo".to_owned())]
                }),
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("b".to_owned())]
                }),
            ]
        );
    }

    #[test]
    fn a_hash_inside_a_word_is_literal() {
        assert_eq!(single_word("a#b"), vec![RawPart::Literal("a#b".to_owned())]);
    }

    #[test]
    fn single_quotes_are_fully_literal() {
        assert_eq!(
            single_word(r#"'$VAR $(cmd) \n'"#),
            vec![RawPart::SingleQuoted(r"$VAR $(cmd) \n".to_owned())]
        );
    }

    #[test]
    fn double_quotes_interpolate_parameters_and_substitutions() {
        assert_eq!(
            single_word(r#""a ${NAME} $(echo b)""#),
            vec![RawPart::DoubleQuoted(vec![
                RawPart::Literal("a ".to_owned()),
                RawPart::Parameter(RawParameter::Named {
                    name: "NAME".to_owned(),
                    indices: Vec::new(),
                }),
                RawPart::Literal(" ".to_owned()),
                RawPart::CommandSubstitution("echo b".to_owned()),
            ])]
        );
    }

    #[test]
    fn nested_command_substitution_keeps_its_full_body() {
        assert_eq!(
            single_word("$(echo $(echo inner) ')' )"),
            vec![RawPart::CommandSubstitution(
                "echo $(echo inner) ')' ".to_owned()
            )]
        );
    }

    #[test]
    fn arithmetic_expansion_is_distinguished_from_command_substitution() {
        assert_eq!(
            single_word("$(( 1 + (2 * 3) ))"),
            vec![RawPart::Arithmetic(" 1 + (2 * 3) ".to_owned())]
        );
    }

    #[test]
    fn indexed_parameters_capture_their_index_word() {
        let parts = single_word("${obj[key]}");
        let RawPart::Parameter(RawParameter::Named { name, indices }) = &parts[0] else {
            panic!("expected an indexed parameter, found {parts:?}");
        };
        assert_eq!(name, "obj");
        assert_eq!(indices.len(), 1);
        assert_eq!(indices[0].as_literal(), Some("key"));
    }

    #[test]
    fn every_dropped_parameter_expansion_is_rejected_by_name() {
        // One case per rejection branch, so a branch that regresses to falling through to
        // `read_name` (where `${#x}` would quietly become the positional count `$#`) fails here.
        for (source, expected) in [
            ("echo ${arr[@]}", "${NAME[@]}"),
            ("echo ${arr[*]}", "${NAME[@]}"),
            ("echo ${#items}", "${#name} length expansion"),
            ("echo ${name:-default}", "keeps only"),
            ("echo ${name/a/b}", "keeps only"),
            ("echo ${name^^}", "keeps only"),
            ("echo ${}", "empty ${} parameter reference"),
            ("echo ${name", "unterminated"),
        ] {
            let error = tokenize(source)
                .map(|tokens| format!("{tokens:?}"))
                .expect_err(source);
            assert!(error.message.contains(expected), "{source}: {error}");
        }
    }

    #[test]
    fn positional_parameters_cover_at_hash_and_star() {
        assert_eq!(
            single_word("$@"),
            vec![RawPart::Parameter(RawParameter::AllPositional)]
        );
        assert_eq!(
            single_word("$*"),
            vec![RawPart::Parameter(RawParameter::AllPositionalJoined)]
        );
        assert_eq!(
            single_word("${*}"),
            vec![RawPart::Parameter(RawParameter::AllPositionalJoined)]
        );
        assert_eq!(
            single_word("$#"),
            vec![RawPart::Parameter(RawParameter::PositionalCount)]
        );
    }

    #[test]
    fn backtick_substitution_is_rejected_by_name() {
        for source in ["echo `echo hi`", r#"echo "`echo hi`""#, "x=`date`"] {
            let error = tokenize(source).expect_err("backticks are dropped");
            assert!(error.message.contains("backtick"), "{source}: {error}");
            assert!(error.message.contains("$( ... )"), "{source}: {error}");
        }
        // An escaped backtick is ordinary text in both bash and here.
        assert_eq!(single_word(r"\`"), vec![RawPart::Literal("`".to_owned())]);
        assert_eq!(
            single_word("'`echo hi`'"),
            vec![RawPart::SingleQuoted("`echo hi`".to_owned())]
        );
    }

    #[test]
    fn file_descriptor_redirection_is_rejected_by_name() {
        for source in ["echo hi 2>buf", "echo hi 2>&1", "echo hi >&2", "cmd 2>>buf"] {
            let error = tokenize(source).expect_err("fd redirection is dropped");
            assert!(
                error.message.contains("file-descriptor redirection"),
                "{source}: {error}"
            );
        }
        // A digit that is a plain argument, separated from the operator, still redirects normally.
        assert!(kinds("echo 2 > buf").contains(&TokenKind::Great));
    }

    #[test]
    fn case_clause_terminators_are_tokenized_and_fall_through_is_not() {
        assert!(kinds("a;;").contains(&TokenKind::DoubleSemicolon));
        for source in ["a;;&", "a;&"] {
            let error = tokenize(source).expect_err("fall-through is dropped");
            assert!(error.message.contains("falls through"), "{source}: {error}");
        }
    }

    #[test]
    fn a_here_document_collects_the_lines_after_its_operator() {
        let TokenKind::HereDoc(body) = &kinds("cat <<EOF\nhello\nthere\nEOF\n")[1] else {
            panic!("expected a here-document token");
        };
        assert_eq!(
            body.parts,
            vec![RawPart::Literal("hello\nthere".to_owned())]
        );
    }

    #[test]
    fn an_unquoted_here_document_interpolates_and_a_quoted_one_does_not() {
        let TokenKind::HereDoc(expanded) = &kinds("cat <<EOF\nid=$id\nEOF\n")[1] else {
            panic!("expected a here-document token");
        };
        assert_eq!(
            expanded.parts,
            vec![
                RawPart::Literal("id=".to_owned()),
                RawPart::Parameter(RawParameter::Named {
                    name: "id".to_owned(),
                    indices: Vec::new(),
                }),
            ]
        );

        // Any quoting of the delimiter turns the whole body literal, as in bash.
        for source in [
            "cat <<'EOF'\nid=$id\nEOF\n",
            "cat <<\"EOF\"\nid=$id\nEOF\n",
            "cat <<\\EOF\nid=$id\nEOF\n",
        ] {
            let TokenKind::HereDoc(literal) = &kinds(source)[1] else {
                panic!("expected a here-document token for {source:?}");
            };
            assert_eq!(
                literal.parts,
                vec![RawPart::Literal("id=$id".to_owned())],
                "{source}"
            );
        }
    }

    #[test]
    fn a_here_document_body_keeps_backslashes_that_json_depends_on() {
        // `\"` is an escaped quote inside double quotes and ordinary text in a here-document.
        // Collapsing it here would silently rewrite embedded JSON.
        let TokenKind::HereDoc(body) = &kinds("cat <<EOF\n{\"a\": \"\\\"x\\\"\"}\nEOF\n")[1] else {
            panic!("expected a here-document token");
        };
        assert_eq!(
            body.parts,
            vec![RawPart::Literal("{\"a\": \"\\\"x\\\"\"}".to_owned())]
        );
    }

    #[test]
    fn a_dash_here_document_strips_leading_tabs_from_body_and_terminator() {
        let TokenKind::HereDoc(body) = &kinds("cat <<-EOF\n\t\tindented\n\tEOF\n")[1] else {
            panic!("expected a here-document token");
        };
        assert_eq!(body.parts, vec![RawPart::Literal("indented".to_owned())]);

        // Only tabs, never spaces: a space-indented terminator does not close the document.
        assert!(tokenize("cat <<-EOF\nbody\n    EOF\n").is_err());
    }

    #[test]
    fn the_rest_of_the_operator_line_is_ordinary_shell() {
        // `cat <<EOF | jq .` must keep working: the body starts on the next line, not immediately.
        let tokens = kinds("cat <<EOF | jq .\n{\"a\":1}\nEOF\n");
        assert!(matches!(tokens[1], TokenKind::HereDoc(_)));
        assert!(tokens.contains(&TokenKind::Pipe));
    }

    #[test]
    fn several_here_documents_on_one_line_read_their_bodies_in_order() {
        let tokens = kinds("f <<A <<B\nfirst\nA\nsecond\nB\n");
        let bodies = tokens
            .iter()
            .filter_map(|token| match token {
                TokenKind::HereDoc(body) => Some(body.parts.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            bodies,
            vec![
                vec![RawPart::Literal("first".to_owned())],
                vec![RawPart::Literal("second".to_owned())],
            ]
        );
    }

    #[test]
    fn malformed_here_documents_are_rejected_by_name() {
        for (source, expected) in [
            ("cat <<EOF\nbody\n", "unterminated here-document"),
            ("cat <<EOF", "unterminated here-document"),
            ("cat <<\n", "expected a here-document delimiter"),
            ("cat <<<\"$x\"", "here-string"),
        ] {
            let error = tokenize(source).expect_err(source);
            assert!(error.message.contains(expected), "{source}: {error}");
        }
    }

    #[test]
    fn braces_are_reserved_words_only_as_complete_words() {
        assert_eq!(
            kinds("f() { echo hi; }"),
            vec![
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("f".to_owned())]
                }),
                TokenKind::LeftParen,
                TokenKind::RightParen,
                TokenKind::LeftBrace,
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("echo".to_owned())]
                }),
                TokenKind::Word(super::RawWord {
                    parts: vec![RawPart::Literal("hi".to_owned())]
                }),
                TokenKind::Semicolon,
                TokenKind::RightBrace,
            ]
        );
        // Brace expansion is dropped, so `{a,b}` stays one literal word.
        assert_eq!(
            single_word("{a,b}"),
            vec![RawPart::Literal("{a,b}".to_owned())]
        );
    }

    #[test]
    fn globbing_characters_are_ordinary_literals() {
        assert_eq!(single_word("*"), vec![RawPart::Literal("*".to_owned())]);
        assert_eq!(
            single_word("a?[b]~"),
            vec![RawPart::Literal("a?[b]~".to_owned())]
        );
    }

    #[test]
    fn redirection_and_backgrounding_operators_are_tokenized_not_dropped() {
        assert!(kinds("echo hi > buf").contains(&TokenKind::Great));
        assert!(kinds("echo hi >> buf").contains(&TokenKind::GreatGreat));
        assert!(kinds("sleep 1 &").contains(&TokenKind::Ampersand));
        assert!(kinds("cat < f").contains(&TokenKind::Less));
        assert!(kinds("diff <(a) b").contains(&TokenKind::LessParen));
    }

    #[test]
    fn unterminated_quotes_are_reported_with_a_line() {
        let error = tokenize("echo 'open").expect_err("unterminated quote");
        assert_eq!(error.line, 1);
        assert!(error.message.contains("unterminated"), "{error}");
    }
}
