use super::parse::{AwkError, Refusal, name_refusal};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Punct {
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Question,
    Colon,
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
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Not,
    Inc,
    Dec,
    Dollar,
    Append,
    Pipe,
}
#[derive(Debug, PartialEq)]
pub(super) enum TokenKind {
    Number(f64),
    String(String),
    Name(String),
    Punct(Punct),
    Newline,
    Eof,
}
#[derive(Debug)]
pub(super) struct Token {
    pub kind: TokenKind,
    pub line: usize,
    pub column: usize,
}
pub(super) struct Lexer<'a> {
    source: &'a str,
    position: usize,
    line: usize,
    column: usize,
}

impl<'a> Lexer<'a> {
    pub(super) fn new(source: &'a str) -> Self {
        Self {
            source,
            position: 0,
            line: 1,
            column: 1,
        }
    }
    fn peek(&self) -> Option<char> {
        self.source[self.position..].chars().next()
    }
    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.position += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(ch)
    }
    fn error<T>(&self, expected: &str) -> Result<T, AwkError> {
        Err(AwkError::Syntax {
            line: self.line,
            column: self.column,
            expected: expected.into(),
        })
    }
    pub(super) fn next(&mut self) -> Result<Token, AwkError> {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r' | '\u{000b}' | '\u{000c}') => {
                    self.bump();
                }
                Some('#') => {
                    while self.peek().is_some_and(|ch| ch != '\n') {
                        self.bump();
                    }
                }
                Some('\\') if self.source[self.position..].starts_with("\\\n") => {
                    self.bump();
                    self.bump();
                }
                Some(_) | None => break,
            }
        }
        let line = self.line;
        let column = self.column;
        let Some(ch) = self.peek() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                line,
                column,
            });
        };
        let kind = if ch == '\n' {
            self.bump();
            TokenKind::Newline
        } else if ch == '"' {
            self.bump();
            TokenKind::String(decode_escapes(
                &self.quoted('"', "a closing string quote")?,
            )?)
        } else if ch.is_ascii_alphabetic() || ch == '_' || ch == '@' {
            let start = self.position;
            self.bump();
            while self
                .peek()
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            {
                self.bump();
            }
            let name = &self.source[start..self.position];
            if let Some(refusal) = name_refusal(name) {
                return Err(refusal.into());
            }
            if ch == '@' {
                return self
                    .error("an identifier (only @include and @load are recognized extensions)");
            }
            TokenKind::Name(name.into())
        } else if ch.is_ascii_digit()
            || (ch == '.'
                && self.source[self.position + 1..].starts_with(|ch: char| ch.is_ascii_digit()))
        {
            TokenKind::Number(self.number()?)
        } else {
            if self.source[self.position..].starts_with("**") {
                return Err(Refusal::PowerSpelling.into());
            }
            let punctuation = [
                ("+=", Punct::AddAssign),
                ("-=", Punct::SubAssign),
                ("*=", Punct::MulAssign),
                ("/=", Punct::DivAssign),
                ("%=", Punct::ModAssign),
                ("^=", Punct::PowAssign),
                ("||", Punct::Or),
                ("&&", Punct::And),
                ("!~", Punct::NotMatch),
                ("<=", Punct::Le),
                ("==", Punct::Eq),
                ("!=", Punct::Ne),
                (">=", Punct::Ge),
                ("++", Punct::Inc),
                ("--", Punct::Dec),
                (">>", Punct::Append),
                ("{", Punct::LBrace),
                ("}", Punct::RBrace),
                ("(", Punct::LParen),
                (")", Punct::RParen),
                ("[", Punct::LBracket),
                ("]", Punct::RBracket),
                (";", Punct::Semi),
                (",", Punct::Comma),
                ("?", Punct::Question),
                (":", Punct::Colon),
                ("=", Punct::Assign),
                ("~", Punct::Match),
                ("<", Punct::Lt),
                (">", Punct::Gt),
                ("+", Punct::Add),
                ("-", Punct::Sub),
                ("*", Punct::Mul),
                ("/", Punct::Div),
                ("%", Punct::Mod),
                ("^", Punct::Pow),
                ("!", Punct::Not),
                ("$", Punct::Dollar),
                ("|", Punct::Pipe),
            ];
            let Some((text, punct)) = punctuation
                .iter()
                .find(|(text, _)| self.source[self.position..].starts_with(text))
            else {
                return self.error("a number, string, identifier or awk operator");
            };
            for _ in text.chars() {
                self.bump();
            }
            TokenKind::Punct(*punct)
        };
        Ok(Token { kind, line, column })
    }
    fn number(&mut self) -> Result<f64, AwkError> {
        let start = self.position;
        if self.source[start..].starts_with("0x") || self.source[start..].starts_with("0X") {
            return self.error("a decimal number (hexadecimal literals are not supported)");
        }
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                self.bump();
            }
        }
        if self.peek() == Some('e') || self.peek() == Some('E') {
            let suffix = &self.source[self.position + 1..];
            let suffix = suffix.strip_prefix(['+', '-']).unwrap_or(suffix);
            if suffix.starts_with(|ch: char| ch.is_ascii_digit()) {
                self.bump();
                if self.peek() == Some('+') || self.peek() == Some('-') {
                    self.bump();
                }
                while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                    self.bump();
                }
            }
        }
        self.source[start..self.position]
            .parse()
            .map_err(|cause| AwkError::Syntax {
                line: self.line,
                column: self.column,
                expected: format!("a decimal number: {cause}"),
            })
    }
    fn quoted(&mut self, delimiter: char, expected: &str) -> Result<String, AwkError> {
        let mut text = String::new();
        while let Some(ch) = self.bump() {
            if ch == delimiter {
                return Ok(text);
            }
            if ch == '\n' {
                return self.error(expected);
            }
            text.push(ch);
            if ch == '\\' {
                let Some(escaped) = self.bump() else {
                    return self.error(expected);
                };
                if escaped == '\n' {
                    text.pop();
                } else {
                    text.push(escaped);
                }
            }
        }
        self.error(expected)
    }
    pub(super) fn regex(&mut self, prefix: &str) -> Result<String, AwkError> {
        let mut source = prefix.to_owned();
        source.push_str(&self.quoted('/', "a closing regex slash")?);
        Ok(source)
    }
}

fn escaped(ch: char) -> Option<char> {
    match ch {
        '"' => Some('"'),
        '/' => Some('/'),
        'a' => Some('\u{0007}'),
        'b' => Some('\u{0008}'),
        'f' => Some('\u{000c}'),
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        'v' => Some('\u{000b}'),
        _ => None,
    }
}
fn octal(first: char, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> char {
    let mut value = u32::from(first) - u32::from('0');
    for _ in 0..2 {
        if let Some(ch @ '0'..='7') = chars.peek().copied() {
            chars.next();
            value = value * 8 + u32::from(ch) - u32::from('0');
        } else {
            break;
        }
    }
    char::from_u32(value).unwrap_or('\0')
}
pub(super) fn decode_escapes(source: &str) -> Result<String, AwkError> {
    let mut output = String::new();
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => output.push('\\'),
            Some(ch @ '0'..='7') => output.push(octal(ch, &mut chars)),
            Some(ch) => match escaped(ch) {
                Some(literal) => output.push(literal),
                None => {
                    output.push('\\');
                    output.push(ch);
                }
            },
            None => output.push('\\'),
        }
    }
    Ok(output)
}

pub(super) fn regex_source(source: &str) -> Result<String, AwkError> {
    let mut chars = source.chars().peekable();
    let mut output = String::new();
    let mut bracket = false;
    let mut first = false;
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some(next) => {
                    let literal = if ('0'..='7').contains(&next) {
                        Some(octal(next, &mut chars))
                    } else {
                        escaped(next)
                    };
                    if let Some(literal) = literal {
                        if "\\.^$|?*+()[]{}-".contains(literal) {
                            output.push('\\');
                        }
                        output.push(literal);
                    } else {
                        output.push('\\');
                        output.push(next);
                    }
                }
                None => output.push('\\'),
            }
            first = false;
            continue;
        }
        if bracket {
            output.push(ch);
            if ch == '['
                && let Some(marker @ (':' | '=' | '.')) = chars.peek().copied()
            {
                output.push(marker);
                chars.next();
                while let Some(inner) = chars.next() {
                    output.push(inner);
                    if inner == marker && chars.peek() == Some(&']') {
                        output.push(']');
                        chars.next();
                        break;
                    }
                }
            } else if ch == ']' {
                if !first {
                    bracket = false;
                }
            } else if ch == '^' && first {
                continue;
            }
            first = false;
        } else if ch == '[' {
            output.push(ch);
            bracket = true;
            first = true;
        } else if ch == '(' {
            if chars.peek() == Some(&'?') {
                return Err(Refusal::RegexExtension.into());
            }
            output.push_str("(?:");
        } else {
            output.push(ch);
        }
    }
    Ok(output)
}
