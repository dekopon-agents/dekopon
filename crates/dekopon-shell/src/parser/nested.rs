use std::ops::{Deref, DerefMut};

use super::*;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Construct {
    If,
    Elif,
    Arithmetic,
    While,
    Until,
    For,
    Case,
    Group,
    Function,
    Conditional,
    TestNot,
    TestGroup,
}

impl Construct {
    fn units(self) -> u32 {
        match self {
            Self::Group => 2,
            Self::If
            | Self::Elif
            | Self::Arithmetic
            | Self::While
            | Self::Until
            | Self::For
            | Self::Case
            | Self::Function
            | Self::Conditional
            | Self::TestNot
            | Self::TestGroup => 1,
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Elif => "an `elif` branch",
            Self::Arithmetic => "an arithmetic expansion",
            Self::If => "an `if` statement",
            Self::While => "a `while` loop",
            Self::Until => "an `until` loop",
            Self::For => "a `for` loop",
            Self::Case => "a `case` statement",
            Self::Group => "a `{ ...; }` group",
            Self::Function => "a function body",
            Self::Conditional => "a `[[ ... ]]` conditional",
            Self::TestNot => "a `!` inside `[[ ... ]]`",
            Self::TestGroup => "a group inside `[[ ... ]]`",
        }
    }
}

pub(super) struct Nested<'p, 'a> {
    parser: &'p mut Parser<'a>,
    saved: Depth,
}

impl<'a> Parser<'a> {
    pub(super) fn nest(&mut self, construct: Construct) -> Result<Nested<'_, 'a>, ParseError> {
        let saved = self.depth;
        self.depth = saved.nest(construct.units(), self.line(), construct)?;
        Ok(Nested {
            parser: self,
            saved,
        })
    }
}

impl<'a> Deref for Nested<'_, 'a> {
    type Target = Parser<'a>;
    fn deref(&self) -> &Self::Target {
        self.parser
    }
}

impl DerefMut for Nested<'_, '_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.parser
    }
}

impl Drop for Nested<'_, '_> {
    fn drop(&mut self) {
        self.parser.depth = self.saved;
    }
}

impl Nested<'_, '_> {
    pub(super) fn parse_if(&mut self) -> Result<IfStatement, ParseError> {
        self.expect_reserved("if", Construct::If)?;
        let mut branches = Vec::new();
        let condition = self.parse_and_or_list()?;
        self.skip_separators();
        self.expect_reserved("then", Construct::If)?;
        let body = self.parse_program(&["elif", "else", "fi"])?;
        branches.push((condition, body));

        let mut otherwise = None;
        loop {
            match self.peek_reserved() {
                Some("elif") => {
                    self.position += 1;
                    let condition = self.parse_and_or_list()?;
                    self.skip_separators();
                    self.expect_reserved("then", Construct::Elif)?;
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
        self.expect_reserved("fi", Construct::If)?;
        Ok(IfStatement {
            branches,
            otherwise,
        })
    }

    pub(super) fn parse_while(&mut self, until: bool) -> Result<WhileLoop, ParseError> {
        let (keyword, context) = if until {
            ("until", Construct::Until)
        } else {
            ("while", Construct::While)
        };
        self.expect_reserved(keyword, context)?;
        let condition = self.parse_and_or_list()?;
        self.skip_separators();
        self.expect_reserved("do", context)?;
        let body = self.parse_program(&["done"])?;
        self.expect_reserved("done", context)?;
        Ok(WhileLoop {
            condition,
            body,
            until,
        })
    }

    pub(super) fn parse_group(&mut self) -> Result<Program, ParseError> {
        let line = self.line();
        self.position += 1;
        let body = self.parse_program(&["}"])?;
        if !matches!(self.peek_kind(), Some(TokenKind::RightBrace)) {
            return Err(ParseError::refused(line, Refusal::ExpectedGroupClose));
        }
        self.position += 1;
        if body.statements.is_empty() {
            return Err(ParseError::refused(line, Refusal::EmptyGroup));
        }
        Ok(body)
    }

    pub(super) fn parse_conditional(&mut self) -> Result<Conditional, ParseError> {
        let line = self.line();
        self.position += 1;
        let expression = self.parse_conditional_or()?;
        if !self.peek_literal_word("]]") {
            let found = self.peek_kind().cloned();
            return Err(ParseError::refused(
                line,
                Refusal::ExpectedConditionalClose {
                    found: Found(found),
                },
            ));
        }
        self.position += 1;
        Ok(expression)
    }

    pub(super) fn parse_case_body(
        &mut self,
        patterns: Vec<CasePattern>,
    ) -> Result<CaseClause, ParseError> {
        let body = self.parse_program(&["esac"])?;
        if matches!(self.peek_kind(), Some(TokenKind::DoubleSemicolon)) {
            self.position += 1;
        } else if self.peek_reserved() != Some("esac") {
            let line = self.line();
            return Err(ParseError::refused(line, Refusal::ExpectedCaseClauseEnd));
        }
        Ok(CaseClause { patterns, body })
    }

    pub(super) fn parse_function_body(&mut self) -> Result<Program, ParseError> {
        self.parse_program(&[])
    }

    pub(super) fn parse_for_body(
        &mut self,
        variable: String,
        words: Vec<Word>,
    ) -> Result<ForLoop, ParseError> {
        let body = self.parse_program(&["done"])?;
        self.expect_reserved("done", Construct::For)?;
        Ok(ForLoop {
            variable,
            words,
            body,
        })
    }

    pub(super) fn parse_test_not(&mut self) -> Result<Conditional, ParseError> {
        self.position += 1;
        let inner = self.parse_conditional_unary()?;
        Ok(Conditional::Not(Box::new(inner)))
    }

    pub(super) fn parse_test_group(&mut self) -> Result<Conditional, ParseError> {
        let line = self.line();
        self.position += 1;
        let inner = self.parse_conditional_or()?;
        if !matches!(self.peek_kind(), Some(TokenKind::RightParen)) {
            return Err(ParseError::refused(line, Refusal::ExpectedTestGroupClose));
        }
        self.position += 1;
        Ok(inner)
    }
}

pub(super) struct ArithmeticGroup<'p> {
    parser: &'p mut ArithParser,
    saved: Depth,
}

impl ArithParser {
    pub(super) fn group(&mut self) -> Result<ArithmeticGroup<'_>, ParseError> {
        let saved = self.depth;
        self.depth = saved.arithmetic_group(self.line)?;
        Ok(ArithmeticGroup {
            parser: self,
            saved,
        })
    }
}

impl Deref for ArithmeticGroup<'_> {
    type Target = ArithParser;
    fn deref(&self) -> &Self::Target {
        self.parser
    }
}
impl DerefMut for ArithmeticGroup<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.parser
    }
}
impl Drop for ArithmeticGroup<'_> {
    fn drop(&mut self) {
        self.parser.depth = self.saved;
    }
}
