use super::{
    AwkError,
    parse::{GawkSyntax, Refusal},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Symbol {
    Open,
    Close,
    Brace,
    EndBrace,
    Bracket,
    EndBracket,
    Comma,
    Semi,
    Dollar,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Not,
    Match,
    NotMatch,
    Lt,
    Le,
    Eq,
    Ne,
    Gt,
    Ge,
    And,
    Or,
    Question,
    Colon,
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    PowAssign,
    Increment,
    Decrement,
    Append,
    Pipe,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Kind {
    Name(String),
    Number(f64),
    String(String),
    Symbol(Symbol),
    Newline,
    End,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Token {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub column: usize,
}

impl Token {
    pub fn error(&self, expected: impl Into<String>) -> AwkError {
        AwkError::Syntax {
            line: self.line,
            column: self.column,
            expected: expected.into(),
        }
    }
}

#[derive(Clone)]
pub(super) struct Lexer<'a> {
    source: &'a str,
    offset: usize,
    line: usize,
    column: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            offset: 0,
            line: 1,
            column: 1,
        }
    }
    fn peek(&self) -> Option<char> {
        self.source[self.offset..].chars().next()
    }
    fn take(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.offset += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }
    fn token(&self, kind: Kind, start: usize, line: usize, column: usize) -> Token {
        Token {
            kind,
            start,
            end: self.offset,
            line,
            column,
        }
    }

    pub fn next(&mut self) -> Result<Token, AwkError> {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r') => {
                    self.take();
                }
                Some('#') => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.take();
                    }
                }
                Some('\\') if self.source[self.offset..].starts_with("\\\n") => {
                    self.take();
                    self.take();
                }
                Some(_) | None => break,
            }
        }
        let (start, line, column) = (self.offset, self.line, self.column);
        let Some(c) = self.take() else {
            return Ok(self.token(Kind::End, start, line, column));
        };
        let kind = match c {
            '\n' => Kind::Newline,
            '"' => Kind::String(self.string(line, column)?),
            c if c.is_ascii_alphabetic() || c == '_' => {
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    self.take();
                }
                Kind::Name(self.source[start..self.offset].to_owned())
            }
            c if c.is_ascii_digit()
                || c == '.' && self.peek().is_some_and(|c| c.is_ascii_digit()) =>
            {
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.take();
                }
                if c != '.' && self.peek() == Some('.') {
                    self.take();
                    while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                        self.take();
                    }
                }
                if self.peek().is_some_and(|c| c == 'e' || c == 'E') {
                    let mut chars = self.source[self.offset + 1..].chars().peekable();
                    if chars.peek().is_some_and(|c| *c == '+' || *c == '-') {
                        chars.next();
                    }
                    if chars.peek().is_some_and(char::is_ascii_digit) {
                        self.take();
                        if self.peek().is_some_and(|c| c == '+' || c == '-') {
                            self.take();
                        }
                        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                            self.take();
                        }
                    }
                }
                let number = self.source[start..self.offset].parse().map_err(
                    |error: std::num::ParseFloatError| {
                        self.token(Kind::End, start, line, column)
                            .error(format!("a decimal number ({error})"))
                    },
                )?;
                Kind::Number(number)
            }
            '@' => {
                while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
                    self.take();
                }
                let name = &self.source[start..self.offset];
                if name == "@include" {
                    return Err(Refusal::GawkSyntax(GawkSyntax::Include).into());
                }
                if name == "@load" {
                    return Err(Refusal::GawkSyntax(GawkSyntax::Load).into());
                }
                return Err(self
                    .token(Kind::End, start, line, column)
                    .error("an awk token"));
            }
            c => Kind::Symbol(self.symbol(c, start, line, column)?),
        };
        Ok(self.token(kind, start, line, column))
    }

    fn symbol(
        &mut self,
        c: char,
        start: usize,
        line: usize,
        column: usize,
    ) -> Result<Symbol, AwkError> {
        use Symbol as S;
        let pair = match (c, self.peek()) {
            ('*', Some('*')) => return Err(Refusal::PowerSpelling.into()),
            ('+', Some('+')) => Some(S::Increment),
            ('-', Some('-')) => Some(S::Decrement),
            ('+', Some('=')) => Some(S::AddAssign),
            ('-', Some('=')) => Some(S::SubAssign),
            ('*', Some('=')) => Some(S::MulAssign),
            ('/', Some('=')) => Some(S::DivAssign),
            ('%', Some('=')) => Some(S::RemAssign),
            ('^', Some('=')) => Some(S::PowAssign),
            ('=', Some('=')) => Some(S::Eq),
            ('!', Some('=')) => Some(S::Ne),
            ('<', Some('=')) => Some(S::Le),
            ('>', Some('=')) => Some(S::Ge),
            ('!', Some('~')) => Some(S::NotMatch),
            ('&', Some('&')) => Some(S::And),
            ('|', Some('|')) => Some(S::Or),
            ('>', Some('>')) => Some(S::Append),
            _ => None,
        };
        if let Some(symbol) = pair {
            self.take();
            return Ok(symbol);
        }
        Ok(match c {
            '(' => S::Open,
            ')' => S::Close,
            '{' => S::Brace,
            '}' => S::EndBrace,
            '[' => S::Bracket,
            ']' => S::EndBracket,
            ',' => S::Comma,
            ';' => S::Semi,
            '$' => S::Dollar,
            '+' => S::Add,
            '-' => S::Sub,
            '*' => S::Mul,
            '/' => S::Div,
            '%' => S::Rem,
            '^' => S::Pow,
            '!' => S::Not,
            '~' => S::Match,
            '<' => S::Lt,
            '>' => S::Gt,
            '?' => S::Question,
            ':' => S::Colon,
            '=' => S::Assign,
            '|' => S::Pipe,
            _ => {
                return Err(self
                    .token(Kind::End, start, line, column)
                    .error("an awk token"));
            }
        })
    }

    fn string(&mut self, line: usize, column: usize) -> Result<String, AwkError> {
        let mut source = String::new();
        while let Some(c) = self.take() {
            match c {
                '"' => return Ok(escapes(&source)),
                '\n' => break,
                '\\' => match self.take() {
                    Some('\n') => {}
                    Some(next) => {
                        source.push(c);
                        source.push(next);
                    }
                    None => break,
                },
                c => source.push(c),
            }
        }
        Err(AwkError::Syntax {
            line,
            column,
            expected: "a closing double quote".into(),
        })
    }

    pub fn regex(&mut self, token: &Token, prefix: &str) -> Result<String, AwkError> {
        let mut source = prefix.to_owned();
        let mut bracket = Bracket::Outside;
        while let Some(c) = self.take() {
            if c == '\n' {
                break;
            }
            if c == '\\' {
                match self.take() {
                    Some('\n') => continue,
                    Some(next) => {
                        source.push(c);
                        source.push(next);
                        bracket.character(next, true);
                        continue;
                    }
                    None => break,
                }
            }
            if c == '/' && bracket == Bracket::Outside {
                return Ok(source);
            }
            source.push(c);
            if bracket.starts_class(c, self.peek()) {
                if let Some(marker) = self.take() {
                    source.push(marker);
                    bracket = Bracket::Class(marker);
                }
            } else {
                bracket.character(c, false);
            }
        }
        Err(token.error("a closing slash for the regular expression"))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Bracket {
    Outside,
    First,
    AfterCaret,
    Inside,
    Class(char),
    ClassEnd(char),
}

impl Bracket {
    fn starts_class(self, c: char, next: Option<char>) -> bool {
        match self {
            Self::First | Self::AfterCaret | Self::Inside => {
                c == '[' && next.is_some_and(|c| ":=.".contains(c))
            }
            Self::Outside | Self::Class(_) | Self::ClassEnd(_) => false,
        }
    }
    fn character(&mut self, c: char, escaped: bool) {
        *self = match *self {
            Self::Outside => {
                if c == '[' && !escaped {
                    Self::First
                } else {
                    Self::Outside
                }
            }
            Self::First if c == '^' && !escaped => Self::AfterCaret,
            Self::First | Self::AfterCaret => Self::Inside,
            Self::Inside if c == ']' && !escaped => Self::Outside,
            Self::Inside => Self::Inside,
            Self::Class(marker) => {
                if c == marker && !escaped {
                    Self::ClassEnd(marker)
                } else {
                    Self::Class(marker)
                }
            }
            Self::ClassEnd(marker) => {
                if c == ']' && !escaped {
                    Self::Inside
                } else {
                    Self::Class(marker)
                }
            }
        };
    }
}

fn escaped(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<(char, bool)> {
    let c = chars.next()?;
    let value = match c {
        '"' => '"',
        '/' => '/',
        '\\' => return Some(('\\', false)),
        'a' => '\x07',
        'b' => '\x08',
        'f' => '\x0c',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\x0b',
        '0'..='7' => {
            let mut number = c as u32 - '0' as u32;
            for _ in 0..2 {
                if let Some(next) = chars.peek().copied().filter(|c| ('0'..='7').contains(c)) {
                    number = number * 8 + next as u32 - '0' as u32;
                    chars.next();
                } else {
                    break;
                }
            }
            char::from_u32(number).unwrap_or('\0')
        }
        c => return Some((c, false)),
    };
    Some((value, true))
}

pub(super) fn escapes(source: &str) -> String {
    let mut output = String::new();
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            output.push(c);
            continue;
        }
        match escaped(&mut chars) {
            Some((c, translated)) => {
                if !translated && c != '\\' {
                    output.push('\\');
                }
                output.push(c);
            }
            None => output.push('\\'),
        }
    }
    output
}

pub(super) fn rewrite_regex(source: &str) -> Result<String, Refusal> {
    let mut chars = source.chars().peekable();
    let mut output = String::new();
    let mut bracket = Bracket::Outside;
    while let Some(c) = chars.next() {
        if c == '\\' {
            output.push('\\');
            if let Some((c, translated)) = escaped(&mut chars) {
                if translated && !"\\.^$|?*+()[]{}-".contains(c) {
                    output.pop();
                }
                output.push(c);
                bracket.character(c, true);
            }
            continue;
        }
        if c == '(' && bracket == Bracket::Outside {
            if chars.peek() == Some(&'?') {
                return Err(Refusal::RegexExtension);
            }
            output.push_str("(?:");
        } else {
            output.push(c);
        }
        if bracket.starts_class(c, chars.peek().copied()) {
            if let Some(marker) = chars.next() {
                output.push(marker);
                bracket = Bracket::Class(marker);
            }
        } else {
            bracket.character(c, false);
        }
    }
    Ok(output)
}

pub(super) fn validate_format(function: &str, format: &str) -> Result<(), AwkError> {
    let mut chars = format.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '%' {
            continue;
        }
        if chars.peek().is_some_and(|(_, c)| *c == '%') {
            chars.next();
            continue;
        }
        while chars.peek().is_some_and(|(_, c)| "-+ #0".contains(*c)) {
            chars.next();
        }
        for dimension in ["width", "precision"] {
            if dimension == "precision" {
                if !chars.peek().is_some_and(|(_, c)| *c == '.') {
                    break;
                }
                chars.next();
            }
            if chars.peek().is_some_and(|(_, c)| *c == '*') {
                return Err(Refusal::FormatStar.into());
            }
            let start = chars.peek().map_or(format.len(), |(at, _)| *at);
            let mut value = 0_u32;
            let mut end = start;
            while let Some((at, c)) = chars.peek().copied().filter(|(_, c)| c.is_ascii_digit()) {
                value = value
                    .saturating_mul(10)
                    .saturating_add(c as u32 - '0' as u32);
                end = at + 1;
                chars.next();
            }
            if value > 4096 {
                return Err(Refusal::FormatBound {
                    function: function.into(),
                    dimension: dimension.into(),
                    value: format[start..end].into(),
                }
                .into());
            }
        }
        if let Some((_, c)) = chars.next() {
            if c == '*' {
                return Err(Refusal::FormatStar.into());
            }
            if !"cdiouxXeEfFgGs%".contains(c) {
                return Err(Refusal::FormatConversion(c).into());
            }
        } else {
            return Err(AwkError::Syntax {
                line: 1,
                column: format.chars().count() + 1,
                expected: "a conversion after `%`".into(),
            });
        }
    }
    Ok(())
}
