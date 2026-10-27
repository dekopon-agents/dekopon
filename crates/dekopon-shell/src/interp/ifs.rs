use std::{collections::HashSet, ops::Range};

use dekopon_process::{LimitExceeded, RetainedBytes};

use crate::{builtins::CommandFailure, limits::Budget};

pub(super) struct Ifs {
    delimiters: HashSet<char>,
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
        Ok(Self {
            delimiters: text.chars().collect(),
            _charge: charge,
        })
    }

    pub(super) fn check_quoted(&self, text: &str) -> Result<(), CommandFailure> {
        let mut bytes = [false; 256];
        for character in &self.delimiters {
            let mut encoded = [0; 4];
            for byte in character.encode_utf8(&mut encoded).bytes() {
                bytes[usize::from(byte)] = true;
            }
        }
        for character in text.chars().filter(|character| !character.is_ascii()) {
            let mut encoded = [0; 4];
            if character
                .encode_utf8(&mut encoded)
                .bytes()
                .skip(1)
                .any(|byte| bytes[usize::from(byte)])
            {
                return Err(super::invalid_ifs_utf8());
            }
        }
        Ok(())
    }

    fn kind(&self, character: char) -> Kind {
        if !self.delimiters.contains(&character) {
            Kind::Text
        } else if matches!(character, ' ' | '\t' | '\n') {
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
            .map(move |(offset, character)| {
                while protected.peek().is_some_and(|range| range.end <= offset) {
                    protected.next();
                }
                let kind = if protected
                    .peek()
                    .is_some_and(|range| range.contains(&offset))
                {
                    Kind::Text
                } else {
                    self.kind(character)
                };
                (kind, offset..offset + character.len_utf8())
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
        let mut boundary = Boundary::Start;
        let mut ranges: Vec<Range<usize>> = Vec::new();
        let mut retained = budget.charge_value_bytes(0)?;
        let mut end = 0;
        for (kind, range) in self.scan(text, protected) {
            if kind != Kind::Whitespace {
                end = range.end;
            }
            let partial_delimiter = kind == Kind::Delimiter && boundary == Boundary::Whitespace;
            let delimiter = range.clone();
            match boundary.accept(kind) {
                Action::Start | Action::Empty => {
                    budget.charge_step()?;
                    retained.grow(size_of::<Range<usize>>() as u64)?;
                    ranges.push(if kind == Kind::Delimiter {
                        range.start..range.start
                    } else {
                        range
                    });
                }
                Action::Append => {
                    if let Some(last) = ranges.last_mut() {
                        last.end = range.end;
                    }
                }
                Action::Separate => {}
            }
            if partial_delimiter {
                for offset in 1..delimiter.len() {
                    budget.charge_step()?;
                    retained.grow(size_of::<Range<usize>>() as u64)?;
                    let start = delimiter.start + offset;
                    ranges.push(start..start);
                }
            }
        }
        if ranges.len() > count {
            ranges.truncate(count);
            if let Some(last) = ranges.last_mut() {
                last.end = end;
            }
        }
        Ok(ReadFields {
            ranges,
            _charge: retained,
        })
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
