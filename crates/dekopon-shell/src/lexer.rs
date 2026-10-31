use std::{collections::VecDeque, fmt, iter::Peekable, str::CharIndices};

use crate::{
    ast::{Index, Modifier, Parameter, Stream, Word, WordPart},
    parser::{self, ParseError},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub line: usize,
    pub offset: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Word(Word),
    Pipe,
    Semicolon,
    DoubleSemicolon,
    Newline,
    AndAnd,
    OrOr,
    Ampersand,
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    Redirect { source: Stream, append: bool },
    Duplicate { source: Stream, target: Stream },
    Less,
    HereDoc(Word),
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
            Self::Redirect { source, append } => {
                return write!(
                    formatter,
                    "{}{}",
                    source.descriptor(),
                    if *append { ">>" } else { ">" }
                );
            }
            Self::Duplicate { source, target } => {
                return write!(
                    formatter,
                    "{}>&{}",
                    source.descriptor(),
                    target.descriptor()
                );
            }
            Self::Less => "<",
            Self::LessParen => "<(",
        };
        formatter.write_str(rendered)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LexError {
    Backtick,
    UnterminatedHereDocument {
        delimiter: String,
    },
    TrailingBackslash,
    UnterminatedSingleQuoted,
    HereString,
    UnterminatedHereDelimiter,
    SplitHereDelimiter,
    ExpectedHereDelimiter,
    PositionalOutOfRange {
        digits: String,
    },
    EmptyParameter,
    IndexAfterAll {
        name: String,
    },
    UnterminatedParameter,
    LengthWithTransformation {
        name: String,
        first: char,
    },
    ColonInPattern {
        name: String,
        operator: char,
    },
    UnsupportedParameter {
        name: String,
        colon: bool,
        other: char,
    },
    OperatorInParameter {
        operator: Option<TokenKind>,
        character: char,
    },
    UnterminatedIndex,
    EmptyIndex,
    MultipleIndexExpressions,
    UnterminatedArithmetic,
    ArithmeticClosingText,
    DuplicationTarget,
    UnknownDescriptor,
    InputDuplication,
    BothDuplication,
    CaseFallthrough,
    UnterminatedDoubleQuoted,
    UnterminatedHereBody,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backtick => formatter.write_str("backtick command substitution is not supported; use `$( ... )`, which nests and quotes cleanly"),
            Self::UnterminatedHereDocument { delimiter } => write!(formatter, "unterminated here-document: the script ended before a line containing exactly {:?}", delimiter),
            Self::TrailingBackslash => formatter.write_str("script ends with a trailing backslash"),
            Self::UnterminatedSingleQuoted => formatter.write_str("unterminated single-quoted string"),
            Self::HereString => formatter.write_str("the here-string `<<<` is not supported; pipe the value instead, as in `echo \"$x\" | cmd`, or use a here-document `<<EOF ... EOF`"),
            Self::UnterminatedHereDelimiter => formatter.write_str("unterminated quoted here-document delimiter"),
            Self::SplitHereDelimiter => formatter.write_str("a here-document delimiter cannot be split across lines; write it on the same line as `<<`"),
            Self::ExpectedHereDelimiter => formatter.write_str("expected a here-document delimiter after `<<`, as in `cat <<EOF`"),
            Self::PositionalOutOfRange { digits } => write!(formatter, "positional parameter ${digits} is out of range"),
            Self::EmptyParameter => formatter.write_str("empty ${} parameter reference"),
            Self::IndexAfterAll { name } => write!(formatter, "${{{name}[@]}} selects every element, so it cannot be indexed further"),
            Self::UnterminatedParameter => formatter.write_str("unterminated ${} parameter reference"),
            Self::LengthWithTransformation { name, first } => write!(formatter, "${{#{name}{first}...}} combines a length with a transformation; ask for one or the other"),
            Self::ColonInPattern { name, operator } => write!(formatter, "`${{{name}:{operator}...}}` is not a parameter expansion; drop the colon"),
            Self::UnsupportedParameter { name, colon, other } => write!(formatter,
                        "unsupported ${{{name}{}{other}...}} parameter expansion; this shell keeps \
                         ${{NAME}}, ${{NAME[index]}}, ${{#NAME}}, `:-`, `:=`, `:?`, `:+`, `#`, `%`, \
                         and `/`",
                        if *colon { ":" } else { "" }
                    ),
            Self::OperatorInParameter { operator, character } => write!(formatter, "`{}` is not allowed inside a ${{}} parameter expansion", operator.as_ref().map_or_else(|| character.to_string(), ToString::to_string)),
            Self::UnterminatedIndex => formatter.write_str("unterminated ${NAME[index]} reference"),
            Self::EmptyIndex => formatter.write_str("empty ${NAME[index]} reference"),
            Self::MultipleIndexExpressions => formatter.write_str("${NAME[index]} accepts exactly one index expression"),
            Self::UnterminatedArithmetic => formatter.write_str("unterminated $(( ... ))"),
            Self::ArithmeticClosingText => formatter.write_str("unexpected text between the closing parentheses of $(( ... ))"),
            Self::DuplicationTarget => formatter.write_str("a duplication must name a stream: write `>&1` or `>&2`; to write a buffer use `> name`, `2> name`, or `&> name`"),
            Self::UnknownDescriptor => formatter.write_str("only descriptors 1 (the value stream) and 2 (the diagnostic stream) exist in this shell; there is nothing else to redirect"),
            Self::InputDuplication => formatter.write_str("input duplication (`<&`) is not supported: there is no input descriptor to duplicate; pipe a value or `cat` a named buffer instead"),
            Self::BothDuplication => formatter.write_str("`&>&` is not a redirection: `&>` already sends both streams to one buffer, so there is no second stream left to duplicate"),
            Self::CaseFallthrough => formatter.write_str("`;&` and `;;&` are not supported: a `case` clause here runs alone and never falls through to the next; end every clause with `;;`"),
            Self::UnterminatedDoubleQuoted => formatter.write_str("unterminated double-quoted string"),
            Self::UnterminatedHereBody => formatter.write_str("unterminated here-document"),
        }
    }
}

struct PendingHereDoc {
    delimiter: String,
    strip_tabs: bool,
    expand: bool,
    line: usize,
    token: usize,
}

#[derive(Clone, Copy)]
enum Enclosure {
    Bare,
    DoubleQuoted,
}

#[cfg(test)]
pub fn tokenize(source: &str) -> Result<Vec<Token>, ParseError> {
    Lexer::new(source, crate::parser::depth::test_depth(), 1).run()
}

pub(crate) struct Lexer<'a> {
    source: &'a str,
    chars: Peekable<CharIndices<'a>>,
    line: usize,
    tokens: VecDeque<Token>,
    parts: Vec<WordPart>,
    literal: String,
    word_started: bool,
    word_line: usize,
    word_offset: usize,
    token_offset: usize,
    pending_here_docs: Vec<PendingHereDoc>,
    pending_here_doc_close: Option<usize>,
    depth: parser::depth::Depth,
}

impl<'a> Lexer<'a> {
    pub(crate) fn new(source: &'a str, depth: parser::depth::Depth, start_line: usize) -> Self {
        Self {
            source,
            chars: source.char_indices().peekable(),
            line: start_line,
            tokens: VecDeque::new(),
            parts: Vec::new(),
            literal: String::new(),
            word_started: false,
            word_line: start_line,
            word_offset: 0,
            token_offset: 0,
            pending_here_docs: Vec::new(),
            pending_here_doc_close: None,
            depth,
        }
    }

    fn run(mut self) -> Result<Vec<Token>, ParseError> {
        let mut tokens = Vec::new();
        while let Some(token) = self.next_token()? {
            tokens.push(token);
        }
        Ok(tokens)
    }

    pub(crate) fn set_parser_depth(&mut self, depth: parser::depth::Depth) {
        self.depth = depth;
    }

    pub(crate) fn pending_here_doc_close(&self) -> Option<usize> {
        self.pending_here_doc_close
    }

    pub(crate) fn offset(&mut self) -> usize {
        self.chars
            .peek()
            .map_or(self.source.len(), |(index, _)| *index)
    }

    pub(crate) fn next_token(&mut self) -> Result<Option<Token>, ParseError> {
        if !self.tokens.is_empty() && self.pending_here_docs.is_empty() {
            return Ok(self.tokens.pop_front());
        }
        while let Some((index, character)) = self.chars.next() {
            self.token_offset = index;
            match character {
                '\n' => {
                    self.finish_word();
                    self.push(TokenKind::Newline);
                    self.line += 1;
                    self.read_pending_here_doc_bodies()?;
                }
                ' ' | '\t' | '\r' => self.finish_word(),
                '#' if !self.word_started => self.skip_comment(),
                '\\' => self.read_escape()?,
                '\'' => self.read_single_quoted()?,
                '"' => self.read_double_quoted()?,
                '$' => self.read_dollar(index, Enclosure::Bare)?,
                // Backticks are rejected by name rather than falling through as literal text, since
                // silently treating them as text would make a script look like its command had run.
                '`' => return Err(ParseError::lexical(self.line, LexError::Backtick)),
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
            if !self.tokens.is_empty() && self.pending_here_docs.is_empty() {
                return Ok(self.tokens.pop_front());
            }
        }
        self.finish_word();
        if let Some(pending) = self.pending_here_docs.first() {
            return Err(ParseError::lexical(
                pending.line,
                LexError::UnterminatedHereDocument {
                    delimiter: pending.delimiter.clone(),
                },
            ));
        }
        Ok(self.tokens.pop_front())
    }

    fn push(&mut self, kind: TokenKind) {
        let line = self.line;
        let offset = self.token_offset;
        self.tokens.push_back(Token { kind, line, offset });
    }

    fn push_literal(&mut self, character: char) {
        self.begin_word();
        self.literal.push(character);
    }

    fn begin_word(&mut self) {
        if !self.word_started {
            self.word_started = true;
            self.word_line = self.line;
            self.word_offset = self.token_offset;
        }
    }

    fn flush_literal(&mut self) {
        if !self.literal.is_empty() {
            let literal = std::mem::take(&mut self.literal);
            self.parts.push(WordPart::Literal(literal));
        }
    }

    fn push_part(&mut self, part: WordPart) {
        self.begin_word();
        self.flush_literal();
        self.parts.push(part);
    }

    fn finish_word(&mut self) {
        if !self.word_started {
            return;
        }
        self.flush_literal();
        let mut parts = std::mem::take(&mut self.parts);
        if let [WordPart::Literal(text)] = parts.as_slice()
            && let Some(number) = dekopon_core::AssetNumber::parse(text)
        {
            parts = vec![WordPart::Asset(number)];
        }
        let line = self.word_line;
        self.word_started = false;
        self.tokens.push_back(Token {
            kind: TokenKind::Word(Word { parts }),
            line,
            offset: self.word_offset,
        });
    }

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

    fn read_escape(&mut self) -> Result<(), ParseError> {
        match self.chars.next() {
            Some((_, '\n')) => {
                self.line += 1;
                Ok(())
            }
            Some((_, character)) => {
                // An escaped character is recorded as a single-quoted part, not merged into literal
                // text, so a case pattern can tell an escaped asterisk from the bare wildcard
                // default, which would otherwise capture everything.
                self.push_part(WordPart::SingleQuoted(character.to_string()));
                Ok(())
            }
            None => Err(ParseError::lexical(self.line, LexError::TrailingBackslash)),
        }
    }

    fn read_single_quoted(&mut self) -> Result<(), ParseError> {
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
                    return Err(ParseError::lexical(
                        opened,
                        LexError::UnterminatedSingleQuoted,
                    ));
                }
            }
        }
        self.push_part(WordPart::SingleQuoted(text));
        Ok(())
    }

    fn read_double_quoted(&mut self) -> Result<(), ParseError> {
        let parts = self.read_interpolated(Some('"'), LexError::UnterminatedDoubleQuoted)?;
        self.push_part(WordPart::DoubleQuoted(parts));
        Ok(())
    }

    fn read_interpolated(
        &mut self,
        terminator: Option<char>,
        unterminated: LexError,
    ) -> Result<Vec<WordPart>, ParseError> {
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
                return Err(ParseError::lexical(opened, unterminated.clone()));
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
                        return Err(ParseError::lexical(opened, unterminated.clone()));
                    }
                },
                '$' => {
                    let part = self.read_dollar_part(index, Enclosure::DoubleQuoted)?;
                    match part {
                        Some(part) => {
                            if !literal.is_empty() {
                                parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                            }
                            parts.push(part);
                        }
                        None => literal.push('$'),
                    }
                }
                '`' => return Err(ParseError::lexical(self.line, LexError::Backtick)),
                other => {
                    if other == '\n' {
                        self.line += 1;
                    }
                    literal.push(other);
                }
            }
        }
        if !literal.is_empty() {
            parts.push(WordPart::Literal(literal));
        }
        Ok(parts)
    }

    fn read_here_doc_header(&mut self) -> Result<(), ParseError> {
        if self.chars.peek().map(|(_, character)| *character) == Some('<') {
            return Err(ParseError::lexical(self.line, LexError::HereString));
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
        self.tokens.push_back(Token {
            kind: TokenKind::HereDoc(Word { parts: Vec::new() }),
            line,
            offset: self.token_offset,
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

    fn read_here_doc_delimiter(&mut self) -> Result<(String, bool), ParseError> {
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
                                return Err(ParseError::lexical(
                                    line,
                                    LexError::UnterminatedHereDelimiter,
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
                        // Folding a line continuation's newline into the here-document delimiter
                        // would create a terminator no body line could match, swallowing the rest
                        // of the script and skewing later line numbers.
                        Some((_, '\n')) => {
                            return Err(ParseError::lexical(line, LexError::SplitHereDelimiter));
                        }
                        Some((_, escaped)) => delimiter.push(escaped),
                        None => {
                            return Err(ParseError::lexical(line, LexError::TrailingBackslash));
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
            return Err(ParseError::lexical(line, LexError::ExpectedHereDelimiter));
        }
        Ok((delimiter, !quoted))
    }

    fn read_pending_here_doc_bodies(&mut self) -> Result<(), ParseError> {
        let pending = std::mem::take(&mut self.pending_here_docs);
        self.pending_here_doc_close = None;
        for specification in pending {
            let body_line = self.line;
            let mut body = self.read_here_doc_body(&specification)?;
            // Values in this shell are not newline-terminated, so the last body line's newline is
            // dropped; keeping it would print an extra trailing blank line bash's heredoc does not
            // produce.
            body.pop();
            let parts = if specification.expand {
                Self::interpolate_here_doc_body(&body, body_line, self.depth)?
            } else if body.is_empty() {
                Vec::new()
            } else {
                vec![WordPart::Literal(body)]
            };
            self.tokens[specification.token].kind = TokenKind::HereDoc(Word { parts });
        }
        Ok(())
    }

    fn read_here_doc_body(&mut self, specification: &PendingHereDoc) -> Result<String, ParseError> {
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

            // The <<- form strips leading tabs only, never spaces, from both body lines and the
            // terminator, letting a here-document be indented with its surrounding block.
            let content = if specification.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line.as_str()
            };
            if content == specification.delimiter {
                return Ok(body);
            }
            if !terminated {
                return Err(ParseError::lexical(
                    specification.line,
                    LexError::UnterminatedHereDocument {
                        delimiter: specification.delimiter.clone(),
                    },
                ));
            }
            body.push_str(content);
            body.push('\n');
        }
    }

    fn interpolate_here_doc_body(
        body: &str,
        line: usize,
        depth: parser::depth::Depth,
    ) -> Result<Vec<WordPart>, ParseError> {
        let mut lexer = Lexer::new(body, depth, line);
        lexer.read_interpolated(None, LexError::UnterminatedHereBody)
    }

    fn read_dollar(&mut self, index: usize, enclosure: Enclosure) -> Result<(), ParseError> {
        match self.read_dollar_part(index, enclosure)? {
            Some(part) => self.push_part(part),
            None => self.push_literal('$'),
        }
        Ok(())
    }

    fn read_dollar_part(
        &mut self,
        dollar_index: usize,
        enclosure: Enclosure,
    ) -> Result<Option<WordPart>, ParseError> {
        let Some((_, next)) = self.chars.peek().copied() else {
            return Ok(None);
        };
        match next {
            '(' => {
                self.chars.next();
                if self.chars.peek().map(|(_, character)| *character) == Some('(') {
                    self.chars.next();
                    let line = self.line;
                    let body = self.read_balanced(dollar_index, '(', ')', 2)?;
                    return Ok(Some(WordPart::Arithmetic(parser::parse_arithmetic(
                        &body, line, self.depth,
                    )?)));
                }
                let start = self.offset();
                let (program, consumed) =
                    parser::parse_substitution(&self.source[start..], self.depth, self.line)?;
                let end = start + consumed;
                while self.chars.peek().is_some_and(|(index, _)| *index < end) {
                    if let Some((_, '\n')) = self.chars.next() {
                        self.line += 1;
                    }
                }
                Ok(Some(WordPart::CommandSubstitution(program)))
            }
            '{' => {
                self.chars.next();
                self.read_braced_parameter(enclosure).map(Some)
            }
            '?' => {
                self.chars.next();
                Ok(Some(WordPart::Parameter(Parameter::LastStatus)))
            }
            '!' => {
                self.chars.next();
                Ok(Some(WordPart::Parameter(Parameter::LastJob)))
            }
            '@' => {
                self.chars.next();
                Ok(Some(WordPart::Parameter(Parameter::AllPositional)))
            }
            '*' => {
                self.chars.next();
                Ok(Some(WordPart::Parameter(Parameter::AllPositionalJoined)))
            }
            '#' => {
                self.chars.next();
                Ok(Some(WordPart::Parameter(Parameter::PositionalCount)))
            }
            digit if digit.is_ascii_digit() => {
                self.chars.next();
                let position = usize::from(
                    digit
                        .to_digit(10)
                        .and_then(|value| u8::try_from(value).ok())
                        .unwrap_or_default(),
                );
                Ok(Some(WordPart::Parameter(Parameter::Positional(position))))
            }
            first if first.is_ascii_alphabetic() || first == '_' => {
                let name = self.read_name();
                Ok(Some(WordPart::Parameter(Parameter::Named {
                    name,
                    indices: Vec::new(),
                    modifier: Modifier::None,
                    length: false,
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

    fn read_braced_parameter(&mut self, enclosure: Enclosure) -> Result<WordPart, ParseError> {
        let line = self.line;
        match self.chars.peek().map(|(_, character)| *character) {
            Some('?') => {
                self.chars.next();
                self.expect_brace_close(line)?;
                return Ok(WordPart::Parameter(Parameter::LastStatus));
            }
            Some('@') => {
                self.chars.next();
                self.expect_brace_close(line)?;
                return Ok(WordPart::Parameter(Parameter::AllPositional));
            }
            Some('*') => {
                self.chars.next();
                self.expect_brace_close(line)?;
                return Ok(WordPart::Parameter(Parameter::AllPositionalJoined));
            }
            Some('#') => {
                self.chars.next();
                if self.chars.peek().map(|(_, character)| *character) == Some('}') {
                    self.chars.next();
                    return Ok(WordPart::Parameter(Parameter::PositionalCount));
                }
                return self.read_named_parameter(line, true, enclosure);
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
                #[allow(
                    clippy::map_err_ignore,
                    reason = "the loop above accepted only ASCII digits, so overflow is the one \
                              reachable ParseIntError and the message already says out of range"
                )]
                let position = digits.parse::<usize>().map_err(|_| {
                    ParseError::lexical(line, LexError::PositionalOutOfRange { digits })
                })?;
                return Ok(WordPart::Parameter(Parameter::Positional(position)));
            }
            _ => {}
        }

        self.read_named_parameter(line, false, enclosure)
    }

    fn read_named_parameter(
        &mut self,
        line: usize,
        length: bool,
        enclosure: Enclosure,
    ) -> Result<WordPart, ParseError> {
        let name = self.read_name();
        if name.is_empty() {
            return Err(ParseError::lexical(line, LexError::EmptyParameter));
        }

        let mut indices = Vec::new();
        while self.chars.peek().map(|(_, character)| *character) == Some('[') {
            if matches!(indices.last(), Some(Index::All | Index::AllJoined)) {
                return Err(ParseError::lexical(line, LexError::IndexAfterAll { name }));
            }
            self.chars.next();
            indices.push(self.read_index(line)?);
        }

        let modifier = self.read_modifier(line, &name, length, enclosure)?;
        Ok(WordPart::Parameter(Parameter::Named {
            name,
            indices,
            modifier,
            length,
        }))
    }

    fn read_modifier(
        &mut self,
        line: usize,
        name: &str,
        length: bool,
        enclosure: Enclosure,
    ) -> Result<Modifier, ParseError> {
        let first = match self.chars.peek().map(|(_, character)| *character) {
            Some('}') => {
                self.chars.next();
                return Ok(Modifier::None);
            }
            Some(character) => character,
            None => return Err(ParseError::lexical(line, LexError::UnterminatedParameter)),
        };
        if length {
            return Err(ParseError::lexical(
                line,
                LexError::LengthWithTransformation {
                    name: name.to_owned(),
                    first,
                },
            ));
        }
        self.chars.next();

        let colon = first == ':';
        let operator = if colon {
            match self.chars.next() {
                Some((_, character)) => character,
                None => return Err(ParseError::lexical(line, LexError::UnterminatedParameter)),
            }
        } else {
            first
        };

        // bash removes quotes from a `?` message and from patterns even inside double quotes.
        let modifier = match operator {
            '-' => Modifier::Default {
                colon,
                word: self.read_modifier_word(line, &['}'], enclosure)?.0,
            },
            '=' => Modifier::Assign {
                colon,
                word: self.read_modifier_word(line, &['}'], enclosure)?.0,
            },
            '?' => {
                let word = self.read_modifier_word(line, &['}'], Enclosure::Bare)?.0;
                Modifier::Require {
                    colon,
                    word: (!word.parts.is_empty()).then_some(word),
                }
            }
            '+' => Modifier::Alternate {
                colon,
                word: self.read_modifier_word(line, &['}'], enclosure)?.0,
            },
            '#' | '%' | '/' if colon => {
                return Err(ParseError::lexical(
                    line,
                    LexError::ColonInPattern {
                        name: name.to_owned(),
                        operator,
                    },
                ));
            }
            '#' | '%' => {
                let doubled = self.chars.peek().map(|(_, character)| *character) == Some(operator);
                if doubled {
                    self.chars.next();
                }
                let pattern = self.read_modifier_word(line, &['}'], Enclosure::Bare)?.0;
                if operator == '#' {
                    Modifier::StripPrefix(parser::parameter_pattern(pattern, line)?)
                } else {
                    Modifier::StripSuffix(parser::parameter_pattern(pattern, line)?)
                }
            }
            '/' => {
                let all = self.chars.peek().map(|(_, character)| *character) == Some('/');
                if all {
                    self.chars.next();
                }
                let (pattern, terminator) =
                    self.read_modifier_word(line, &['/', '}'], Enclosure::Bare)?;
                let replacement = if terminator == '/' {
                    self.read_modifier_word(line, &['}'], Enclosure::Bare)?.0
                } else {
                    Word { parts: Vec::new() }
                };
                Modifier::Replace {
                    all,
                    pattern: parser::parameter_pattern(pattern, line)?,
                    replacement,
                }
            }
            other => {
                return Err(ParseError::lexical(
                    line,
                    LexError::UnsupportedParameter {
                        name: name.to_owned(),
                        colon,
                        other,
                    },
                ));
            }
        };
        Ok(modifier)
    }

    fn read_modifier_word(
        &mut self,
        line: usize,
        terminators: &[char],
        enclosure: Enclosure,
    ) -> Result<(Word, char), ParseError> {
        if let Some((_, character)) = self.chars.peek().copied()
            && terminators.contains(&character)
        {
            self.chars.next();
            return Ok((Word::default(), character));
        }
        let mut lexer = self.parameter_lexer(self.source, line)?;
        lexer.chars = self.chars.clone();
        lexer.line = self.line;
        let result = lexer.read_parameter_word(line, terminators, enclosure);
        self.chars = lexer.chars;
        self.line = lexer.line;
        result
    }

    fn parameter_lexer<'b>(&self, source: &'b str, line: usize) -> Result<Lexer<'b>, ParseError> {
        let lexer = Lexer::new(source, self.depth.parameter(line)?, self.line);
        Ok(lexer)
    }

    fn read_parameter_word(
        &mut self,
        line: usize,
        terminators: &[char],
        enclosure: Enclosure,
    ) -> Result<(Word, char), ParseError> {
        let mut braces = 0_usize;
        loop {
            let Some((index, character)) = self.chars.next() else {
                return Err(ParseError::lexical(line, LexError::UnterminatedParameter));
            };
            if braces == 0 && terminators.contains(&character) {
                self.flush_literal();
                return Ok((
                    Word {
                        parts: std::mem::take(&mut self.parts),
                    },
                    character,
                ));
            }
            match character {
                '\'' => match enclosure {
                    Enclosure::Bare => self.read_single_quoted()?,
                    Enclosure::DoubleQuoted => self.read_double_quoted_parameter_quote()?,
                },
                '"' => self.read_double_quoted()?,
                '\\' => match enclosure {
                    Enclosure::Bare => self.read_escape()?,
                    Enclosure::DoubleQuoted => self.read_double_quoted_parameter_escape()?,
                },
                '$' => self.read_dollar(index, enclosure)?,
                '`' => return Err(ParseError::lexical(self.line, LexError::Backtick)),
                '|' | '&' | ';' | '<' | '>' | '(' | ')' => {
                    self.read_operator(character)?;
                    let operator = self.tokens.back().map(|token| &token.kind);
                    return Err(ParseError::lexical(
                        line,
                        LexError::OperatorInParameter {
                            operator: operator.cloned(),
                            character,
                        },
                    ));
                }
                other => {
                    match other {
                        '{' => braces += 1,
                        '}' => braces = braces.saturating_sub(1),
                        '\n' => self.line += 1,
                        _ => {}
                    }
                    self.push_literal(other);
                }
            }
        }
    }

    // bash keeps these quotes as text and expands between them, yet a `}` between them does not
    // close the expansion.
    fn read_double_quoted_parameter_quote(&mut self) -> Result<(), ParseError> {
        let opened = self.line;
        self.push_literal('\'');
        loop {
            let Some((index, character)) = self.chars.next() else {
                return Err(ParseError::lexical(
                    opened,
                    LexError::UnterminatedSingleQuoted,
                ));
            };
            match character {
                '\'' => {
                    self.push_literal('\'');
                    return Ok(());
                }
                '\\' if self.chars.peek().map(|(_, next)| *next) == Some('\'') => {
                    self.push_literal('\\');
                }
                '\\' => self.read_double_quoted_parameter_escape()?,
                '"' => self.read_double_quoted()?,
                '$' => self.read_dollar(index, Enclosure::DoubleQuoted)?,
                '`' => return Err(ParseError::lexical(self.line, LexError::Backtick)),
                other => {
                    if other == '\n' {
                        self.line += 1;
                    }
                    self.push_literal(other);
                }
            }
        }
    }

    fn read_double_quoted_parameter_escape(&mut self) -> Result<(), ParseError> {
        match self.chars.next() {
            Some((_, '\n')) => self.line += 1,
            Some((_, escaped @ ('"' | '\\' | '$' | '`' | '}'))) => self.push_literal(escaped),
            Some((_, other)) => {
                self.push_literal('\\');
                self.push_literal(other);
            }
            None => {
                return Err(ParseError::lexical(self.line, LexError::TrailingBackslash));
            }
        }
        Ok(())
    }

    fn expect_brace_close(&mut self, line: usize) -> Result<(), ParseError> {
        match self.chars.next() {
            Some((_, '}')) => Ok(()),
            _ => Err(ParseError::lexical(line, LexError::UnterminatedParameter)),
        }
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn read_index(&mut self, line: usize) -> Result<Index, ParseError> {
        let mut text = String::new();
        let mut depth = 0_usize;
        loop {
            match self.chars.next() {
                Some((_, ']')) if depth == 0 => break,
                Some((_, '\n')) => {
                    return Err(ParseError::lexical(line, LexError::UnterminatedIndex));
                }
                Some((_, character)) => {
                    match character {
                        '[' => depth += 1,
                        ']' => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    text.push(character);
                }
                None => return Err(ParseError::lexical(line, LexError::UnterminatedIndex)),
            }
        }
        match text.as_str() {
            "@" => return Ok(Index::All),
            "*" => return Ok(Index::AllJoined),
            _ => {}
        }
        let tokens = self.parameter_lexer(&text, line)?.run()?;
        let mut words = tokens.into_iter().filter_map(|token| match token.kind {
            TokenKind::Word(word) => Some(word),
            _ => None,
        });
        let word = words
            .next()
            .ok_or_else(|| ParseError::lexical(line, LexError::EmptyIndex))?;
        if words.next().is_some() {
            return Err(ParseError::lexical(
                line,
                LexError::MultipleIndexExpressions,
            ));
        }
        Ok(Index::At(word))
    }

    fn read_balanced(
        &mut self,
        dollar_index: usize,
        open: char,
        close: char,
        mut depth: usize,
    ) -> Result<String, ParseError> {
        let opened = self.line;
        let initial = depth;
        let start = dollar_index + '$'.len_utf8() + open.len_utf8() * depth;
        let mut body_end = None;
        loop {
            let Some((index, character)) = self.chars.next() else {
                return Err(ParseError::lexical(
                    opened,
                    LexError::UnterminatedArithmetic,
                ));
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
                        if index != end + close.len_utf8() {
                            return Err(ParseError::lexical(
                                opened,
                                LexError::ArithmeticClosingText,
                            ));
                        }
                        return Ok(self.source[start..end].to_owned());
                    }
                }
                _ => {}
            }
        }
    }

    fn read_duplication(&mut self, source: Stream) -> Result<(), ParseError> {
        let target = match self.chars.peek().map(|(_, character)| *character) {
            Some('1') => Stream::Stdout,
            Some('2') => Stream::Stderr,
            _ => return Err(ParseError::lexical(self.line, LexError::DuplicationTarget)),
        };
        self.chars.next();
        if self
            .chars
            .peek()
            .is_some_and(|(_, character)| character.is_ascii_digit())
        {
            return Err(ParseError::lexical(self.line, LexError::UnknownDescriptor));
        }
        self.push(TokenKind::Duplicate { source, target });
        Ok(())
    }

    fn read_operator(&mut self, character: char) -> Result<(), ParseError> {
        // A leading descriptor digit, as in 2>, belongs to the redirection operator, not to argv;
        // treating it as an ordinary word would redirect the command's own output where the script
        // never asked.
        let mut source = Stream::Stdout;
        if character == '>'
            && self.word_started
            && self.parts.is_empty()
            && !self.literal.is_empty()
            && self.literal.chars().all(|digit| digit.is_ascii_digit())
        {
            source = match self.literal.as_str() {
                "1" => Stream::Stdout,
                "2" => Stream::Stderr,
                _ => return Err(ParseError::lexical(self.line, LexError::UnknownDescriptor)),
            };
            self.literal.clear();
            self.word_started = false;
        } else if character == '<'
            && self.word_started
            && self.parts.is_empty()
            && !self.literal.is_empty()
            && self.literal.chars().all(|digit| digit.is_ascii_digit())
        {
            return Err(ParseError::lexical(self.line, LexError::InputDuplication));
        }
        self.finish_word();
        let next = self.chars.peek().map(|(_, character)| *character);
        if matches!((character, next), ('<', Some('&'))) {
            return Err(ParseError::lexical(self.line, LexError::InputDuplication));
        }
        if character == '>' && next == Some('&') {
            self.chars.next();
            return self.read_duplication(source);
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
            ('&', Some('>')) => {
                self.chars.next();
                if self.chars.peek().map(|(_, character)| *character) == Some('&') {
                    return Err(ParseError::lexical(self.line, LexError::BothDuplication));
                }
                let append = self.chars.peek().map(|(_, character)| *character) == Some('>');
                if append {
                    self.chars.next();
                }
                TokenKind::Redirect {
                    source: Stream::Both,
                    append,
                }
            }
            ('&', _) => TokenKind::Ampersand,
            (';', Some(';')) => {
                self.chars.next();
                if self.chars.peek().map(|(_, character)| *character) == Some('&') {
                    return Err(ParseError::lexical(self.line, LexError::CaseFallthrough));
                }
                TokenKind::DoubleSemicolon
            }
            (';', Some('&')) => {
                return Err(ParseError::lexical(self.line, LexError::CaseFallthrough));
            }
            (';', _) => TokenKind::Semicolon,
            ('>', Some('>')) => {
                self.chars.next();
                TokenKind::Redirect {
                    source,
                    append: true,
                }
            }
            ('>', _) => TokenKind::Redirect {
                source,
                append: false,
            },
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
            (')', _) => {
                if !self.pending_here_docs.is_empty() && self.pending_here_doc_close.is_none() {
                    self.pending_here_doc_close = Some(self.token_offset);
                }
                TokenKind::RightParen
            }
            _ => unreachable!("read_operator is only called for operator characters"),
        };
        self.push(kind);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Index, LexError, Modifier, Parameter, ParseError, Stream, TokenKind, Word, WordPart,
        tokenize,
    };

    fn kinds(source: &str) -> Vec<TokenKind> {
        tokenize(source)
            .expect("tokenizes")
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn single_word(source: &str) -> Vec<WordPart> {
        match kinds(source).into_iter().next().expect("one token") {
            TokenKind::Word(word) => word.parts,
            other => panic!("expected a word, found {other}"),
        }
    }

    #[test]
    fn an_unquoted_whole_word_is_an_asset_part() {
        assert_eq!(
            single_word("chat-asset:3"),
            [WordPart::Asset(
                dekopon_core::AssetNumber::parse("chat-asset:3").unwrap()
            )]
        );
    }

    #[test]
    fn a_quoted_reference_is_literal_text() {
        assert_eq!(
            single_word("'chat-asset:3'"),
            [WordPart::SingleQuoted("chat-asset:3".to_owned())]
        );
        assert_eq!(
            single_word("\"chat-asset:3\""),
            [WordPart::DoubleQuoted(vec![WordPart::Literal(
                "chat-asset:3".to_owned()
            )])]
        );
    }

    #[test]
    fn an_escaped_reference_is_literal_text() {
        assert_eq!(
            single_word(r"chat\-asset:3"),
            [
                WordPart::Literal("chat".to_owned()),
                WordPart::SingleQuoted("-".to_owned()),
                WordPart::Literal("asset:3".to_owned())
            ]
        );
    }

    #[test]
    fn a_non_canonical_number_is_literal_text() {
        for text in [
            "chat-asset:03",
            "chat-asset:0",
            "chat-asset:18446744073709551616",
            "chat-asset:",
        ] {
            assert_eq!(single_word(text), [WordPart::Literal(text.to_owned())]);
        }
    }

    #[test]
    fn a_reference_inside_a_larger_word_is_literal_text() {
        for text in [
            "chat-asset:3x",
            "xchat-asset:3",
            "--image=chat-asset:3",
            "x=chat-asset:3",
        ] {
            assert_eq!(single_word(text), [WordPart::Literal(text.to_owned())]);
        }
        assert!(matches!(
            single_word("chat-asset:$n").as_slice(),
            [WordPart::Literal(_), WordPart::Parameter(_)]
        ));
    }

    #[test]
    fn a_reference_in_a_substitution_lexes_inside_it() {
        use crate::ast::{Command, Statement};
        let parts = single_word("$(echo chat-asset:3)");
        let [WordPart::CommandSubstitution(program)] = parts.as_slice() else {
            panic!("substitution");
        };
        let Statement::List(list) = &program.statements[0] else {
            panic!("list");
        };
        let Command::Simple(command) = &list.first.commands[0] else {
            panic!("simple command");
        };
        assert!(matches!(
            command.words[1].parts.as_slice(),
            [WordPart::Asset(_)]
        ));
    }

    #[test]
    fn a_here_document_body_is_literal_text() {
        let tokens = kinds("cat <<EOF\nchat-asset:3\nEOF\n");
        assert!(tokens.iter().any(|token| matches!(token, TokenKind::HereDoc(Word { parts }) if parts == &[WordPart::Literal("chat-asset:3".to_owned())])));
    }

    #[test]
    fn a_pending_here_doc_records_a_case_pattern_close() {
        let source = "cat <<EOF; case a in a) `x`";
        let mut lexer = super::Lexer::new(source, crate::parser::depth::test_depth(), 1);
        let error = loop {
            match lexer.next_token() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("must be rejected"),
                Err(error) => break error,
            }
        };
        assert_eq!(error, ParseError::lexical(1, LexError::Backtick));
        assert_eq!(lexer.pending_here_doc_close(), source.find(')'));
    }

    #[test]
    fn a_pending_here_doc_records_a_function_close() {
        let source = "cat <<EOF; f() `x`";
        let mut lexer = super::Lexer::new(source, crate::parser::depth::test_depth(), 1);
        let error = loop {
            match lexer.next_token() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("must be rejected"),
                Err(error) => break error,
            }
        };
        assert_eq!(error, ParseError::lexical(1, LexError::Backtick));
        assert_eq!(lexer.pending_here_doc_close(), source.find(')'));
    }

    #[test]
    fn completed_here_docs_clear_a_pending_function_close() {
        let source = "cat <<EOF; cat <<'END'; f() { true; }\nhi\nEOF\nEND\n`x`";
        let mut lexer = super::Lexer::new(source, crate::parser::depth::test_depth(), 1);
        let error = loop {
            match lexer.next_token() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("must be rejected"),
                Err(error) => break error,
            }
        };
        assert_eq!(error, ParseError::lexical(5, LexError::Backtick));
        assert_eq!(lexer.pending_here_doc_close(), None);
    }

    #[test]
    fn parameter_word_newlines_preserve_following_diagnostic_lines() {
        let error = tokenize("echo \"${x:-a\nb}\"\n${y:^^}").expect_err("unsupported modifier");
        assert_eq!(
            error,
            ParseError::lexical(
                3,
                LexError::UnsupportedParameter {
                    name: "y".to_owned(),
                    colon: true,
                    other: '^'
                }
            )
        );
        assert_eq!(error.line(), 3);
    }

    #[test]
    fn splits_words_and_operators() {
        assert_eq!(
            kinds("echo hi | grep h && true"),
            vec![
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("echo".to_owned())]
                }),
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("hi".to_owned())]
                }),
                TokenKind::Pipe,
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("grep".to_owned())]
                }),
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("h".to_owned())]
                }),
                TokenKind::AndAnd,
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("true".to_owned())]
                }),
            ]
        );
    }

    #[test]
    fn comments_run_to_end_of_line() {
        assert_eq!(
            kinds("echo a # trailing\necho b"),
            vec![
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("echo".to_owned())]
                }),
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("a".to_owned())]
                }),
                TokenKind::Newline,
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("echo".to_owned())]
                }),
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("b".to_owned())]
                }),
            ]
        );
    }

    #[test]
    fn a_hash_inside_a_word_is_literal() {
        assert_eq!(
            single_word("a#b"),
            vec![WordPart::Literal("a#b".to_owned())]
        );
    }

    #[test]
    fn single_quotes_are_fully_literal() {
        assert_eq!(
            single_word(r#"'$VAR $(cmd) \n'"#),
            vec![WordPart::SingleQuoted(r"$VAR $(cmd) \n".to_owned())]
        );
    }

    #[test]
    fn double_quotes_interpolate_parameters_and_substitutions() {
        assert_eq!(
            single_word(r#""a ${NAME} $(echo b)""#),
            vec![WordPart::DoubleQuoted(vec![
                WordPart::Literal("a ".to_owned()),
                WordPart::Parameter(Parameter::Named {
                    name: "NAME".to_owned(),
                    indices: Vec::new(),
                    modifier: Modifier::None,
                    length: false,
                }),
                WordPart::Literal(" ".to_owned()),
                WordPart::CommandSubstitution(crate::parser::parse("echo b").expect("program")),
            ])]
        );
    }

    #[test]
    fn nested_command_substitution_keeps_its_full_body() {
        assert_eq!(
            single_word("$(echo $(echo inner) ')' )"),
            vec![WordPart::CommandSubstitution(
                crate::parser::parse("echo $(echo inner) ')' ").expect("program")
            )]
        );
    }

    #[test]
    fn arithmetic_expansion_is_distinguished_from_command_substitution() {
        assert_eq!(
            single_word("$(( 1 + (2 * 3) ))"),
            vec![WordPart::Arithmetic(
                crate::ast::ArithExpr::binary(
                    crate::ast::ArithBinaryOp::Add,
                    crate::ast::ArithExpr::integer(1),
                    crate::ast::ArithExpr::binary(
                        crate::ast::ArithBinaryOp::Multiply,
                        crate::ast::ArithExpr::integer(2),
                        crate::ast::ArithExpr::integer(3),
                    )
                    .expect("bounded product"),
                )
                .expect("bounded sum")
            )]
        );
    }

    #[test]
    fn indexed_parameters_capture_their_index_word() {
        let parts = single_word("${obj[key]}");
        let WordPart::Parameter(Parameter::Named { name, indices, .. }) = &parts[0] else {
            panic!("expected an indexed parameter, found {parts:?}");
        };
        assert_eq!(name, "obj");
        assert_eq!(
            indices.as_slice(),
            [Index::At(Word {
                parts: vec![WordPart::Literal("key".to_owned())]
            })]
        );

        let parts = single_word("${list[@]}");
        let WordPart::Parameter(Parameter::Named { indices, .. }) = &parts[0] else {
            panic!("expected an indexed parameter, found {parts:?}");
        };
        assert_eq!(indices.as_slice(), [Index::All]);
    }

    #[test]
    fn every_dropped_parameter_expansion_is_rejected_by_name() {
        for (source, expected) in [
            (
                "echo ${arr[@][0]}",
                LexError::IndexAfterAll {
                    name: "arr".to_owned(),
                },
            ),
            (
                "echo ${#items:-x}",
                LexError::LengthWithTransformation {
                    name: "items".to_owned(),
                    first: ':',
                },
            ),
            (
                "echo ${name:#a}",
                LexError::ColonInPattern {
                    name: "name".to_owned(),
                    operator: '#',
                },
            ),
            (
                "echo ${name^^}",
                LexError::UnsupportedParameter {
                    name: "name".to_owned(),
                    colon: false,
                    other: '^',
                },
            ),
            (
                "echo ${name@Q}",
                LexError::UnsupportedParameter {
                    name: "name".to_owned(),
                    colon: false,
                    other: '@',
                },
            ),
            ("echo ${}", LexError::EmptyParameter),
            ("echo ${name", LexError::UnterminatedParameter),
            (
                "echo ${name:-a|b}",
                LexError::OperatorInParameter {
                    operator: Some(TokenKind::Pipe),
                    character: '|',
                },
            ),
            (
                "echo ${name:-a;b}",
                LexError::OperatorInParameter {
                    operator: Some(TokenKind::Semicolon),
                    character: ';',
                },
            ),
            (
                "echo ${name:-a&b}",
                LexError::OperatorInParameter {
                    operator: Some(TokenKind::Ampersand),
                    character: '&',
                },
            ),
            (
                "echo ${name:-a<b}",
                LexError::OperatorInParameter {
                    operator: Some(TokenKind::Less),
                    character: '<',
                },
            ),
            (
                "echo ${name:-a>b}",
                LexError::OperatorInParameter {
                    operator: Some(TokenKind::Redirect {
                        source: Stream::Stdout,
                        append: false,
                    }),
                    character: '>',
                },
            ),
            (
                "echo ${name:-a(b}",
                LexError::OperatorInParameter {
                    operator: Some(TokenKind::LeftParen),
                    character: '(',
                },
            ),
            (
                "echo ${name:-a)b}",
                LexError::OperatorInParameter {
                    operator: Some(TokenKind::RightParen),
                    character: ')',
                },
            ),
        ] {
            let error = tokenize(source)
                .map(|tokens| format!("{tokens:?}"))
                .expect_err(source);
            assert_eq!(error, ParseError::lexical(1, expected), "{source}");
        }
    }

    #[test]
    fn positional_parameters_cover_at_hash_and_star() {
        assert_eq!(
            single_word("$@"),
            vec![WordPart::Parameter(Parameter::AllPositional)]
        );
        assert_eq!(
            single_word("$*"),
            vec![WordPart::Parameter(Parameter::AllPositionalJoined)]
        );
        assert_eq!(
            single_word("${*}"),
            vec![WordPart::Parameter(Parameter::AllPositionalJoined)]
        );
        assert_eq!(
            single_word("$#"),
            vec![WordPart::Parameter(Parameter::PositionalCount)]
        );
    }

    #[test]
    fn backtick_substitution_is_rejected_by_name() {
        for source in ["echo `echo hi`", r#"echo "`echo hi`""#, "x=`date`"] {
            let error = tokenize(source).expect_err("backticks are dropped");
            assert_eq!(error, ParseError::lexical(1, LexError::Backtick));
        }
        assert_eq!(
            single_word(r"\`"),
            vec![WordPart::SingleQuoted("`".to_owned())]
        );
        assert_eq!(
            single_word("'`echo hi`'"),
            vec![WordPart::SingleQuoted("`echo hi`".to_owned())]
        );
    }

    #[test]
    fn file_descriptor_redirection_is_tokenized_per_stream() {
        assert!(kinds("echo hi 2>buf").contains(&TokenKind::Redirect {
            source: Stream::Stderr,
            append: false
        }));
        assert!(kinds("cmd 2>>buf").contains(&TokenKind::Redirect {
            source: Stream::Stderr,
            append: true
        }));
        assert!(kinds("cmd 1> buf").contains(&TokenKind::Redirect {
            source: Stream::Stdout,
            append: false
        }));
        assert!(kinds("cmd &> buf").contains(&TokenKind::Redirect {
            source: Stream::Both,
            append: false
        }));
        assert!(kinds("cmd &>> buf").contains(&TokenKind::Redirect {
            source: Stream::Both,
            append: true
        }));
        assert!(kinds("echo hi 2>&1").contains(&TokenKind::Duplicate {
            source: Stream::Stderr,
            target: Stream::Stdout
        }));
        assert!(kinds("echo hi >&2").contains(&TokenKind::Duplicate {
            source: Stream::Stdout,
            target: Stream::Stderr
        }));

        let separated = kinds("echo 2 > buf");
        assert!(separated.contains(&TokenKind::Redirect {
            source: Stream::Stdout,
            append: false
        }));
        assert!(separated.iter().any(|kind| matches!(
            kind,
            TokenKind::Word(word) if word.parts == vec![WordPart::Literal("2".to_owned())]
        )));

        assert!(!kinds("echo hi 2> buf").iter().any(|kind| matches!(
            kind,
            TokenKind::Word(word) if word.parts == vec![WordPart::Literal("2".to_owned())]
        )));
    }

    #[test]
    fn descriptors_this_shell_does_not_have_are_rejected_by_name() {
        for (source, expected) in [
            ("cmd 3> buf", LexError::UnknownDescriptor),
            ("cmd 2>&3", LexError::DuplicationTarget),
            ("cmd >&12", LexError::UnknownDescriptor),
            ("cmd 0< buf", LexError::InputDuplication),
            ("cmd <& 1", LexError::InputDuplication),
            ("cmd &>& buf", LexError::BothDuplication),
            ("cmd >& buf", LexError::DuplicationTarget),
        ] {
            let error = tokenize(source).expect_err("rejected");
            assert_eq!(error, ParseError::lexical(1, expected), "{source}");
        }
    }

    #[test]
    fn case_clause_terminators_are_tokenized_and_fall_through_is_not() {
        assert!(kinds("a;;").contains(&TokenKind::DoubleSemicolon));
        for source in ["a;;&", "a;&"] {
            let error = tokenize(source).expect_err("fall-through is dropped");
            assert_eq!(error, ParseError::lexical(1, LexError::CaseFallthrough));
        }
    }

    #[test]
    fn a_here_document_collects_the_lines_after_its_operator() {
        let TokenKind::HereDoc(body) = &kinds("cat <<EOF\nhello\nthere\nEOF\n")[1] else {
            panic!("expected a here-document token");
        };
        assert_eq!(
            body.parts,
            vec![WordPart::Literal("hello\nthere".to_owned())]
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
                WordPart::Literal("id=".to_owned()),
                WordPart::Parameter(Parameter::Named {
                    name: "id".to_owned(),
                    indices: Vec::new(),
                    modifier: Modifier::None,
                    length: false,
                }),
            ]
        );

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
                vec![WordPart::Literal("id=$id".to_owned())],
                "{source}"
            );
        }
    }

    #[test]
    fn a_here_document_body_keeps_backslashes_that_json_depends_on() {
        let TokenKind::HereDoc(body) = &kinds("cat <<EOF\n{\"a\": \"\\\"x\\\"\"}\nEOF\n")[1] else {
            panic!("expected a here-document token");
        };
        assert_eq!(
            body.parts,
            vec![WordPart::Literal("{\"a\": \"\\\"x\\\"\"}".to_owned())]
        );
    }

    #[test]
    fn a_dash_here_document_strips_leading_tabs_from_body_and_terminator() {
        let TokenKind::HereDoc(body) = &kinds("cat <<-EOF\n\t\tindented\n\tEOF\n")[1] else {
            panic!("expected a here-document token");
        };
        assert_eq!(body.parts, vec![WordPart::Literal("indented".to_owned())]);

        assert!(tokenize("cat <<-EOF\nbody\n    EOF\n").is_err());
    }

    #[test]
    fn the_rest_of_the_operator_line_is_ordinary_shell() {
        let tokens = kinds("cat <<EOF | jq .\n{\"a\":1}\nEOF\n");
        assert!(matches!(tokens[1], TokenKind::HereDoc(_)));
        assert!(tokens.contains(&TokenKind::Pipe));
    }

    #[test]
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
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
                vec![WordPart::Literal("first".to_owned())],
                vec![WordPart::Literal("second".to_owned())],
            ]
        );
    }

    #[test]
    fn a_diagnostic_inside_a_here_document_body_has_an_absolute_line() {
        let error = tokenize("echo one\ncat <<EOF\nbad `sub`\nEOF\n").expect_err("backticks");
        assert_eq!(error.line(), 3, "{error}");
    }

    #[test]
    fn a_here_document_delimiter_cannot_be_split_across_lines() {
        let error = tokenize("cat <<\\\nEOF\nbody\nEOF\n").expect_err("a split delimiter");
        assert_eq!(error, ParseError::lexical(1, LexError::SplitHereDelimiter));
    }

    #[test]
    fn malformed_here_documents_are_rejected_by_name() {
        for (source, expected) in [
            (
                "cat <<EOF\nbody\n",
                LexError::UnterminatedHereDocument {
                    delimiter: "EOF".to_owned(),
                },
            ),
            (
                "cat <<EOF",
                LexError::UnterminatedHereDocument {
                    delimiter: "EOF".to_owned(),
                },
            ),
            ("cat <<\n", LexError::ExpectedHereDelimiter),
            ("cat <<<\"$x\"", LexError::HereString),
        ] {
            let error = tokenize(source).expect_err(source);
            assert_eq!(error, ParseError::lexical(1, expected), "{source}");
        }
    }

    #[test]
    fn braces_are_reserved_words_only_as_complete_words() {
        assert_eq!(
            kinds("f() { echo hi; }"),
            vec![
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("f".to_owned())]
                }),
                TokenKind::LeftParen,
                TokenKind::RightParen,
                TokenKind::LeftBrace,
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("echo".to_owned())]
                }),
                TokenKind::Word(super::Word {
                    parts: vec![WordPart::Literal("hi".to_owned())]
                }),
                TokenKind::Semicolon,
                TokenKind::RightBrace,
            ]
        );
        assert_eq!(
            single_word("{a,b}"),
            vec![WordPart::Literal("{a,b}".to_owned())]
        );
    }

    #[test]
    fn globbing_characters_are_ordinary_literals() {
        assert_eq!(single_word("*"), vec![WordPart::Literal("*".to_owned())]);
        assert_eq!(
            single_word("a?[b]~"),
            vec![WordPart::Literal("a?[b]~".to_owned())]
        );
    }

    #[test]
    fn redirection_and_backgrounding_operators_are_tokenized_not_dropped() {
        assert!(kinds("echo hi > buf").contains(&TokenKind::Redirect {
            source: Stream::Stdout,
            append: false
        }));
        assert!(kinds("echo hi >> buf").contains(&TokenKind::Redirect {
            source: Stream::Stdout,
            append: true
        }));
        assert!(kinds("sleep 1 &").contains(&TokenKind::Ampersand));
        assert!(kinds("cat < f").contains(&TokenKind::Less));
        assert!(kinds("diff <(a) b").contains(&TokenKind::LessParen));
    }

    #[test]
    fn unterminated_quotes_are_reported_with_a_line() {
        let error = tokenize("echo 'open").expect_err("unterminated quote");
        assert_eq!(error.line(), 1);
        assert_eq!(
            error,
            ParseError::lexical(1, LexError::UnterminatedSingleQuoted)
        );
    }
}
