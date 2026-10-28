use std::ops::Range;

use dekopon_process::{LimitExceeded, RetainedBytes};
use serde_json::Value;

use super::{Evaluator, ExpansionOutput, ParameterExpansion, quoted_text, render_number};
use crate::{
    ast::{Parameter, Word, WordPart},
    builtins::CommandFailure,
    limits::Budget,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PatternFragment {
    Literal(char),
    Unquoted(char),
    QuotedNull,
}

#[derive(Clone, Copy)]
enum Quote {
    Literal,
    Unquoted,
}

#[derive(Clone, Copy)]
enum Context {
    Pattern,
    Replacement,
}

pub(super) struct CaseExpansion {
    fragments: Vec<PatternFragment>,
    charge: RetainedBytes,
    quoted_at: bool,
    first_field_end: Option<usize>,
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
enum BracketRefusal {
    #[error("case pattern uses an unsupported POSIX character class")]
    CharacterClass,
    #[error("case pattern uses an unsupported POSIX collating symbol")]
    CollatingSymbol,
    #[error("case pattern uses an unsupported POSIX equivalence class")]
    EquivalenceClass,
}

#[derive(Debug)]
enum Token {
    Literal(char),
    AnyCharacter,
    AnySequence,
    Bracket {
        members: Range<usize>,
        negated: bool,
    },
}

impl PatternFragment {
    fn character(self) -> char {
        match self {
            Self::Literal(character) | Self::Unquoted(character) => character,
            Self::QuotedNull => '\u{7f}',
        }
    }

    fn is_unquoted(self, expected: char) -> bool {
        match self {
            Self::Unquoted(character) => character == expected,
            Self::Literal(_) | Self::QuotedNull => false,
        }
    }
}

impl CaseExpansion {
    fn new(budget: &Budget) -> Result<Self, LimitExceeded> {
        Ok(Self {
            fragments: Vec::new(),
            charge: budget.charge_value_bytes(0)?,
            quoted_at: false,
            first_field_end: None,
        })
    }

    fn reserve(&mut self, additional: usize) -> Result<(), LimitExceeded> {
        let required = self.fragments.len().saturating_add(additional);
        if required > self.fragments.capacity() {
            let capacity = required.max(self.fragments.capacity().saturating_mul(2));
            self.charge.grow(
                ((capacity - self.fragments.capacity()) as u64)
                    .saturating_mul(size_of::<PatternFragment>() as u64),
            )?;
            self.fragments
                .reserve_exact(capacity - self.fragments.len());
        }
        Ok(())
    }

    fn append(&mut self, text: &str, quote: Quote) -> Result<(), LimitExceeded> {
        self.reserve(text.chars().count())?;
        self.fragments
            .extend(text.chars().map(|character| match quote {
                Quote::Literal => PatternFragment::Literal(character),
                Quote::Unquoted => PatternFragment::Unquoted(character),
            }));
        Ok(())
    }

    fn append_quoted(&mut self, text: &str, context: Context) -> Result<(), LimitExceeded> {
        match context {
            Context::Replacement if text.is_empty() => {
                self.reserve(1)?;
                self.fragments.push(PatternFragment::QuotedNull);
                Ok(())
            }
            Context::Pattern | Context::Replacement => self.append(text, Quote::Literal),
        }
    }

    pub(super) fn append_value(&mut self, value: &Value) -> Result<(), LimitExceeded> {
        self.append(&quoted_text(value), Quote::Unquoted)
    }

    fn normalize(&mut self) {
        if let Some(end) = self.first_field_end {
            self.fragments.truncate(end);
        }
        if self.quoted_at {
            self.fragments.retain(|fragment| match fragment {
                PatternFragment::QuotedNull => false,
                PatternFragment::Literal(_) | PatternFragment::Unquoted(_) => true,
            });
        }
        if self.fragments == [PatternFragment::QuotedNull] {
            self.fragments.clear();
            return;
        }
        let mut escaped = false;
        let mut write = 0;
        for read in 0..self.fragments.len() {
            let fragment = self.fragments[read];
            let character = fragment.character();
            let quoted = match fragment {
                PatternFragment::Literal('\u{1}' | '\u{7f}')
                | PatternFragment::Unquoted(_)
                | PatternFragment::QuotedNull => false,
                PatternFragment::Literal(_) => true,
            };
            if quoted {
                if !escaped {
                    self.fragments[write] = PatternFragment::Literal(character);
                    write += 1;
                    continue;
                }
                self.fragments[write] = PatternFragment::Literal('\\');
                write += 1;
                escaped = false;
            }
            if escaped {
                self.fragments[write] = PatternFragment::Literal(character);
                write += 1;
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else {
                self.fragments[write] = PatternFragment::Unquoted(character);
                write += 1;
            }
        }
        if escaped {
            self.fragments[write] = PatternFragment::Literal('\\');
            write += 1;
        }
        self.fragments.truncate(write);
    }

    fn matches(&mut self, subject: &str, budget: &mut Budget) -> Result<bool, CommandFailure> {
        self.normalize();
        let pattern = &self.fragments;
        let subject_len = subject.chars().count();
        let bytes = (pattern.len() as u64)
            .saturating_mul((size_of::<Token>() + size_of::<Option<usize>>()) as u64)
            .saturating_add(size_of::<Option<usize>>() as u64)
            .saturating_add(
                (subject_len as u64).saturating_mul((size_of::<char>() + size_of::<bool>()) as u64),
            )
            .saturating_add(size_of::<bool>() as u64);
        let _charge = budget.charge_value_bytes(bytes)?;
        let closers = closing_brackets(pattern);
        let tokens = tokenize(pattern, &closers)
            .map_err(|refusal| CommandFailure::usage(refusal.to_string()))?;
        let mut characters = Vec::with_capacity(subject_len);
        characters.extend(subject.chars());
        let subject = characters;
        let mut matched = vec![false; subject.len() + 1];
        matched[0] = true;
        for token in tokens {
            budget.charge_step()?;
            let mut diagonal = matched[0];
            matched[0] = match &token {
                Token::AnySequence => diagonal,
                Token::Literal(_) | Token::AnyCharacter | Token::Bracket { .. } => false,
            };
            for (index, character) in subject.iter().enumerate() {
                let previous = matched[index + 1];
                matched[index + 1] = match &token {
                    Token::AnySequence => previous || matched[index],
                    Token::AnyCharacter => diagonal,
                    Token::Literal(expected) => diagonal && character == expected,
                    Token::Bracket { members, negated } => {
                        diagonal
                            && (bracket_matches(&pattern[members.clone()], *character) != *negated)
                    }
                };
                diagonal = previous;
            }
        }
        Ok(matched[subject.len()])
    }
}

fn closing_brackets(pattern: &[PatternFragment]) -> Vec<Option<usize>> {
    let mut closers = vec![None; pattern.len() + 1];
    for index in (0..pattern.len()).rev() {
        closers[index] = if pattern[index].is_unquoted(']') {
            Some(index)
        } else {
            closers[index + 1]
        };
    }
    closers
}

fn tokenize(
    pattern: &[PatternFragment],
    closers: &[Option<usize>],
) -> Result<Vec<Token>, BracketRefusal> {
    let mut tokens = Vec::with_capacity(pattern.len());
    let mut index = 0;
    while index < pattern.len() {
        let token = match pattern[index] {
            PatternFragment::Unquoted('*') => Token::AnySequence,
            PatternFragment::Unquoted('?') => Token::AnyCharacter,
            PatternFragment::Unquoted('[') => {
                let mut start = index + 1;
                let negated = pattern
                    .get(start)
                    .is_some_and(|fragment| fragment.is_unquoted('!') || fragment.is_unquoted('^'));
                start += usize::from(negated);
                let closing_start = start
                    + usize::from(
                        pattern
                            .get(start)
                            .is_some_and(|fragment| fragment.is_unquoted(']')),
                    );
                if let Some(end) = closers.get(closing_start).copied().flatten() {
                    check_bracket(&pattern[start..end])?;
                    index = end;
                    Token::Bracket {
                        members: start..end,
                        negated,
                    }
                } else {
                    Token::Literal('[')
                }
            }
            PatternFragment::Literal(character) | PatternFragment::Unquoted(character) => {
                Token::Literal(character)
            }
            PatternFragment::QuotedNull => Token::Literal('\u{7f}'),
        };
        tokens.push(token);
        index += 1;
    }
    Ok(tokens)
}

fn check_bracket(members: &[PatternFragment]) -> Result<(), BracketRefusal> {
    for pair in members.windows(2) {
        if !pair[0].is_unquoted('[') {
            continue;
        }
        let refusal = match pair[1] {
            PatternFragment::Unquoted(':') => BracketRefusal::CharacterClass,
            PatternFragment::Unquoted('.') => return Err(BracketRefusal::CollatingSymbol),
            PatternFragment::Unquoted('=') => BracketRefusal::EquivalenceClass,
            PatternFragment::Literal(_)
            | PatternFragment::Unquoted(_)
            | PatternFragment::QuotedNull => continue,
        };
        if members
            .last()
            .is_some_and(|last| last.is_unquoted(pair[1].character()))
        {
            return Err(refusal);
        }
    }
    Ok(())
}

fn bracket_matches(members: &[PatternFragment], subject: char) -> bool {
    let mut index = 0;
    let mut matched = false;
    while index < members.len() {
        let first = members[index].character();
        if index + 2 < members.len() && members[index + 1].is_unquoted('-') {
            matched |= (first..=members[index + 2].character()).contains(&subject);
            index += 3;
        } else {
            matched |= first == subject;
            index += 1;
        }
    }
    matched
}

impl Evaluator<'_> {
    pub(super) fn case_matches(
        &mut self,
        word: &Word,
        subject: &str,
    ) -> Result<bool, CommandFailure> {
        let mut pattern = CaseExpansion::new(&self.budget)?;
        self.expand_case_word(word, &mut pattern, Context::Pattern)?;
        pattern.matches(subject, &mut self.budget)
    }

    pub(super) fn expand_case_replacement(
        &mut self,
        word: &Word,
        pattern: &mut CaseExpansion,
    ) -> Result<Value, CommandFailure> {
        self.expand_case_word(word, pattern, Context::Replacement)?;
        Ok(Value::Null)
    }

    fn expand_case_word(
        &mut self,
        word: &Word,
        pattern: &mut CaseExpansion,
        context: Context,
    ) -> Result<(), CommandFailure> {
        let mut had_quoted_null = false;
        for part in &word.parts {
            match part {
                WordPart::Literal(text) => pattern.append(text, Quote::Unquoted)?,
                WordPart::SingleQuoted(text) => {
                    pattern.append_quoted(text, context)?;
                    had_quoted_null |= text.is_empty();
                }
                WordPart::DoubleQuoted(parts) => {
                    let expanded = self.expand_quoted(parts)?;
                    let text = expanded.fields.first().map_or("", String::as_str);
                    pattern.append_quoted(text, context)?;
                    had_quoted_null |= text.is_empty() || expanded.quoted_at;
                    pattern.quoted_at |= expanded.quoted_at;
                    if expanded.fields.len() > 1 {
                        pattern
                            .first_field_end
                            .get_or_insert(pattern.fragments.len());
                    }
                }
                WordPart::Asset(number) => pattern.append(&number.to_string(), Quote::Unquoted)?,
                WordPart::Arithmetic(expression) => {
                    let number = self.evaluate_arithmetic(expression)?;
                    pattern.append(&render_number(number), Quote::Unquoted)?;
                }
                WordPart::CommandSubstitution(program) => {
                    let result = self.run_substitution(program)?;
                    pattern.append_value(&result.value)?;
                    self.expansion_charges.extend(result.retained);
                }
                WordPart::Parameter(parameter) => {
                    let start = pattern.fragments.len();
                    match parameter {
                        Parameter::Named {
                            name,
                            indices,
                            modifier,
                            length: false,
                        } => {
                            self.nested(|this| {
                                this.named_parameter_value(
                                    name,
                                    indices,
                                    modifier,
                                    Some(ParameterExpansion::Case(pattern)),
                                    ExpansionOutput::Unsplit,
                                )
                            })?;
                        }
                        Parameter::Named { length: true, .. }
                        | Parameter::Positional(_)
                        | Parameter::AllPositional
                        | Parameter::AllPositionalJoined
                        | Parameter::PositionalCount
                        | Parameter::LastStatus
                        | Parameter::LastJob => {
                            let value =
                                self.parameter_value_with(parameter, ExpansionOutput::Unsplit)?;
                            pattern.append_value(&value)?;
                        }
                    }
                    if had_quoted_null
                        && !pattern.quoted_at
                        && pattern.fragments[start..] == [PatternFragment::QuotedNull]
                    {
                        pattern.fragments.truncate(start);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{BracketRefusal, PatternFragment, closing_brackets, tokenize};

    #[test]
    fn unsupported_bracket_constructs_are_typed_refusals() {
        for (source, expected) in [
            ("[[:alpha:]]", BracketRefusal::CharacterClass),
            ("[[.a.]]", BracketRefusal::CollatingSymbol),
            ("[[.a]", BracketRefusal::CollatingSymbol),
            ("[[=a=]]", BracketRefusal::EquivalenceClass),
        ] {
            let pattern: Vec<_> = source.chars().map(PatternFragment::Unquoted).collect();
            assert_eq!(
                tokenize(&pattern, &closing_brackets(&pattern)).expect_err("unsupported bracket"),
                expected
            );
        }
    }
}
