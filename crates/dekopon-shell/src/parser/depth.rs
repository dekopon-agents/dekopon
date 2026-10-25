use super::{MAX_NESTING_DEPTH, ParseError, Program, too_deep};
use crate::lexer::LexError;

const MAX_PARAMETER_NESTING: u32 = 32;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Depth {
    lexer: u32,
    block: u32,
}

pub(super) fn parse(source: &str) -> Result<Program, ParseError> {
    super::parse_nested(source, Depth { lexer: 0, block: 1 })
}

#[cfg(test)]
pub(crate) fn test_depth() -> Depth {
    Depth { lexer: 0, block: 0 }
}

impl Depth {
    pub(super) fn nest(self, units: u32, line: usize, construct: &str) -> Result<Self, ParseError> {
        if self.lexer + self.block + units > MAX_NESTING_DEPTH {
            return Err(too_deep(line, construct));
        }
        Ok(Self {
            block: self.block + units,
            ..self
        })
    }

    pub(super) fn substitution(self, line: usize) -> Result<Self, ParseError> {
        let sum = self.lexer + self.block;
        if sum >= MAX_NESTING_DEPTH - 1 {
            return Err(too_deep(line, "command substitution `$( ... )`"));
        }
        Ok(Self {
            lexer: sum + 1,
            block: 1,
        })
    }

    pub(crate) fn parameter(self, line: usize) -> Result<Self, LexError> {
        if self.lexer + 1 >= MAX_PARAMETER_NESTING {
            return Err(LexError {
                line,
                message: format!("parameter expansions nested deeper than {MAX_PARAMETER_NESTING}"),
            });
        }
        Ok(Self {
            lexer: self.lexer + 1,
            ..self
        })
    }

    pub(super) fn arithmetic_group(self, line: usize) -> Result<Self, ParseError> {
        self.nest(1, line, "an arithmetic expansion")
    }
}
