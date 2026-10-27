use std::{collections::HashSet, ops::Range};

use dekopon_process::{LimitExceeded, RetainedBytes};

use crate::{builtins::CommandFailure, limits::Budget};

pub(super) struct Ifs {
    delimiters: HashSet<char>,
    bytes: [bool; 256],
    _charge: RetainedBytes,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Kind {
    Text,
    Whitespace,
    Delimiter,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Boundary {
    Start,
    ArrayStart,
    Text,
    Whitespace,
    Delimiter,
}

pub(super) enum Action {
    Append,
    Start,
    Empty,
    Separate,
}

impl Boundary {
    pub(super) fn accept(&mut self, kind: Kind) -> Action {
        match kind {
            Kind::Text => {
                let action = match self {
                    Self::Text => Action::Append,
                    Self::Start | Self::ArrayStart | Self::Whitespace | Self::Delimiter => {
                        Action::Start
                    }
                };
                *self = Self::Text;
                action
            }
            Kind::Whitespace => {
                match self {
                    Self::Text | Self::ArrayStart => *self = Self::Whitespace,
                    Self::Start | Self::Whitespace | Self::Delimiter => {}
                }
                Action::Separate
            }
            Kind::Delimiter => {
                let action = match self {
                    Self::Start | Self::ArrayStart | Self::Delimiter => Action::Empty,
                    Self::Text | Self::Whitespace => Action::Separate,
                };
                *self = Self::Delimiter;
                action
            }
        }
    }
}

impl Ifs {
    pub(super) fn new(text: &str, budget: &Budget) -> Result<Self, LimitExceeded> {
        let charge = budget.charge_value_bytes((text.len() * 16) as u64)?;
        let mut bytes = [false; 256];
        for byte in text.bytes() {
            bytes[usize::from(byte)] = true;
        }
        Ok(Self {
            delimiters: text.chars().collect(),
            bytes,
            _charge: charge,
        })
    }

    pub(super) fn check_quoted(&self, text: &str) -> Result<(), CommandFailure> {
        for character in text.chars().filter(|character| !character.is_ascii()) {
            let mut encoded = [0; 4];
            if character
                .encode_utf8(&mut encoded)
                .bytes()
                .skip(1)
                .any(|byte| self.bytes[usize::from(byte)])
            {
                return Err(super::invalid_ifs_utf8());
            }
        }
        Ok(())
    }

    fn kind(&self, character: char) -> Kind {
        if !self.delimiters.contains(&character) {
            Kind::Text
        } else if matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}') {
            Kind::Whitespace
        } else {
            Kind::Delimiter
        }
    }

    pub(super) fn scan<'a>(
        &'a self,
        text: &'a str,
        protected: &'a [Range<usize>],
    ) -> impl Iterator<Item = (Kind, Range<usize>)> + 'a {
        let mut protected = protected.iter().peekable();
        let mut characters = text
            .char_indices()
            .flat_map(move |(offset, character)| {
                while protected.peek().is_some_and(|range| range.end <= offset) {
                    protected.next();
                }
                let quoted = protected
                    .peek()
                    .is_some_and(|range| range.contains(&offset));
                let mut spans: [Option<(Kind, Range<usize>)>; 4] = std::array::from_fn(|_| None);
                if quoted && !character.is_ascii() {
                    for (index, byte) in text.as_bytes()[offset..offset + character.len_utf8()]
                        .iter()
                        .enumerate()
                    {
                        let kind = if index > 0 && self.bytes[usize::from(*byte)] {
                            Kind::Delimiter
                        } else {
                            Kind::Text
                        };
                        spans[index] = Some((kind, offset + index..offset + index + 1));
                    }
                } else {
                    spans[0] = Some((
                        if quoted {
                            Kind::Text
                        } else {
                            self.kind(character)
                        },
                        offset..offset + character.len_utf8(),
                    ));
                }
                spans.into_iter().flatten()
            })
            .peekable();
        std::iter::from_fn(move || {
            let (kind, mut range) = characters.next()?;
            if kind != Kind::Delimiter {
                while characters.peek().is_some_and(|(next, _)| *next == kind) {
                    if let Some((_, next)) = characters.next() {
                        range.end = next.end;
                    }
                }
            }
            Some((kind, range))
        })
    }

    pub(super) fn read_fields(
        &self,
        text: &str,
        protected: &[Range<usize>],
        count: usize,
        budget: &mut Budget,
    ) -> Result<ReadFields, LimitExceeded> {
        let spans = self.scan(text, protected).flat_map(|(kind, range)| {
            let mut offset = range.start;
            std::iter::from_fn(move || {
                if offset == range.end {
                    return None;
                }
                let end = if kind == Kind::Whitespace {
                    offset + 1
                } else {
                    range.end
                };
                let span = (kind, offset..end);
                offset = end;
                Some(span)
            })
        });
        let mut cursor = ReadCursor {
            spans: spans.peekable(),
            continuation: None,
        };
        cursor.skip_whitespace(text, Whitespace::SpaceTabNewline);
        let mut ranges = Vec::new();
        let mut retained = budget.charge_value_bytes(0)?;
        for index in 0..count {
            let remainder = cursor.offset(text.len());
            let mut field = cursor.word(text);
            if index + 1 == count && cursor.offset(text.len()) < text.len() {
                let mut end = text.len();
                let mut escaped = protected.iter().rev().peekable();
                for (offset, character) in text.char_indices().rev() {
                    while escaped.peek().is_some_and(|range| range.start > offset) {
                        escaped.next();
                    }
                    if !matches!(character, ' ' | '\t' | '\n')
                        || self.kind(character) != Kind::Whitespace
                        || escaped.peek().is_some_and(|range| range.contains(&offset))
                    {
                        break;
                    }
                    end = offset;
                }
                field = remainder..end;
            }
            budget.charge_step()?;
            retained.grow(size_of::<Range<usize>>() as u64)?;
            ranges.push(field);
        }
        Ok(ReadFields {
            ranges,
            _charge: retained,
        })
    }
}

#[derive(Clone, Copy)]
enum Whitespace {
    All,
    SpaceTabNewline,
}

struct ReadCursor<I: Iterator<Item = (Kind, Range<usize>)>> {
    spans: std::iter::Peekable<I>,
    continuation: Option<Range<usize>>,
}

impl<I: Iterator<Item = (Kind, Range<usize>)>> ReadCursor<I> {
    fn peek(&mut self) -> Option<(Kind, Range<usize>)> {
        self.continuation.as_ref().map_or_else(
            || self.spans.peek().cloned(),
            |range| Some((Kind::Delimiter, range.start..range.start + 1)),
        )
    }

    fn consume(&mut self) {
        if let Some(range) = &mut self.continuation {
            range.start += 1;
            if range.start == range.end {
                self.continuation = None;
            }
        } else {
            self.spans.next();
        }
    }

    fn consume_byte(&mut self) {
        if self.continuation.is_some() {
            self.consume();
        } else if let Some((_, range)) = self.spans.next()
            && range.len() > 1
        {
            self.continuation = Some(range.start + 1..range.end);
        }
    }

    fn offset(&mut self, end: usize) -> usize {
        self.peek().map_or(end, |(_, range)| range.start)
    }

    fn skip_whitespace(&mut self, text: &str, whitespace: Whitespace) {
        while self.peek().is_some_and(|(kind, range)| {
            kind == Kind::Whitespace
                && match whitespace {
                    Whitespace::All => true,
                    Whitespace::SpaceTabNewline => {
                        matches!(text.as_bytes()[range.start], b' ' | b'\t' | b'\n')
                    }
                }
        }) {
            self.consume();
        }
    }

    fn word(&mut self, text: &str) -> Range<usize> {
        self.skip_whitespace(text, Whitespace::All);
        let start = self.offset(text.len());
        let mut end = start;
        while let Some((Kind::Text, range)) = self.peek() {
            end = range.end;
            self.consume();
        }
        let whitespace = self
            .peek()
            .is_some_and(|(kind, _)| kind == Kind::Whitespace);
        self.consume();
        self.skip_whitespace(text, Whitespace::SpaceTabNewline);
        if whitespace && self.peek().is_some_and(|(kind, _)| kind == Kind::Delimiter) {
            // Bash consumes one byte here, even for a multibyte IFS character.
            self.consume_byte();
            self.skip_whitespace(text, Whitespace::All);
        }
        start..end
    }
}

pub(super) struct ReadFields {
    ranges: Vec<Range<usize>>,
    _charge: RetainedBytes,
}

impl ReadFields {
    pub(super) fn get<'a>(&self, text: &'a str, index: usize) -> Result<&'a str, CommandFailure> {
        match self.ranges.get(index) {
            None => Ok(""),
            Some(range) if range.is_empty() => Ok(""),
            Some(range) => text.get(range.clone()).ok_or_else(super::invalid_ifs_utf8),
        }
    }
}
