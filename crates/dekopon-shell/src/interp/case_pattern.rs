use crate::{builtins::CommandFailure, limits::Budget};

#[derive(Clone, Copy)]
pub(super) enum PatternChar {
    Unquoted(char),
    Literal(char),
}

impl PatternChar {
    fn character(self) -> char {
        match self {
            Self::Unquoted(character) | Self::Literal(character) => character,
        }
    }

    fn is_unquoted(self, expected: char) -> bool {
        match self {
            Self::Unquoted(character) => character == expected,
            Self::Literal(_) => false,
        }
    }
}

enum Token<'a> {
    Literal(char),
    Any,
    Star,
    Bracket {
        members: &'a [PatternChar],
        negated: bool,
    },
}

#[derive(Debug, thiserror::Error)]
enum UnsupportedBracket {
    #[error("`case` POSIX bracket classes are not supported")]
    Class,
    #[error("`case` POSIX collating symbols are not supported")]
    Collating,
    #[error("`case` POSIX equivalence classes are not supported")]
    Equivalence,
}

pub(super) fn matches(
    pattern: &[PatternChar],
    subject: &str,
    budget: &Budget,
) -> Result<bool, CommandFailure> {
    let bytes = pattern
        .len()
        .saturating_mul(
            size_of::<PatternChar>() + size_of::<Option<usize>>() + size_of::<Token<'_>>(),
        )
        .saturating_add(size_of::<Option<usize>>())
        .saturating_add(
            subject
                .len()
                .saturating_mul(size_of::<char>() + 2 * size_of::<bool>()),
        )
        .saturating_add(2 * size_of::<bool>());
    let _scratch = budget.charge_value_bytes(bytes as u64)?;
    let mut normalized = Vec::with_capacity(pattern.len());
    let mut input = pattern.iter().copied();
    let mut pending = None;
    while let Some(character) = pending.take().or_else(|| input.next()) {
        check_deadline(normalized.len(), budget)?;
        match character {
            PatternChar::Unquoted('\\') => match input.next() {
                Some(PatternChar::Unquoted(character)) => {
                    normalized.push(PatternChar::Literal(character))
                }
                Some(PatternChar::Literal(character)) => {
                    normalized.push(PatternChar::Literal('\\'));
                    pending = Some(PatternChar::Unquoted(character));
                }
                None => normalized.push(PatternChar::Literal('\\')),
            },
            PatternChar::Unquoted(_) | PatternChar::Literal(_) => normalized.push(character),
        }
    }
    let mut closers = vec![None; normalized.len() + 1];
    for index in (0..normalized.len()).rev() {
        check_deadline(index, budget)?;
        closers[index] = match normalized[index] {
            PatternChar::Unquoted(']') => Some(index),
            PatternChar::Unquoted(_) | PatternChar::Literal(_) => closers[index + 1],
        };
    }
    let mut tokens = Vec::with_capacity(normalized.len());
    let mut index = 0;
    while index < normalized.len() {
        check_deadline(tokens.len(), budget)?;
        let token = match normalized[index] {
            PatternChar::Unquoted('*') => Token::Star,
            PatternChar::Unquoted('?') => Token::Any,
            PatternChar::Unquoted('[') => {
                let start = index + 1;
                let negated = normalized.get(start).is_some_and(|character| {
                    character.is_unquoted('!') || character.is_unquoted('^')
                });
                let members = start + usize::from(negated);
                let after_initial_close = members
                    + usize::from(
                        normalized
                            .get(members)
                            .is_some_and(|character| character.is_unquoted(']')),
                    );
                if let Some(end) = closers.get(after_initial_close).copied().flatten() {
                    let contents = &normalized[members..end];
                    reject_posix(contents, budget)?;
                    index = end;
                    Token::Bracket {
                        members: contents,
                        negated,
                    }
                } else {
                    Token::Literal('[')
                }
            }
            PatternChar::Unquoted(character) | PatternChar::Literal(character) => {
                Token::Literal(character)
            }
        };
        tokens.push(token);
        index += 1;
    }
    let mut characters = Vec::with_capacity(subject.len());
    for (index, character) in subject.chars().enumerate() {
        check_deadline(index, budget)?;
        characters.push(character);
    }
    let subject = characters;
    let mut previous = vec![false; subject.len() + 1];
    let mut next = vec![false; subject.len() + 1];
    previous[0] = true;
    for token in tokens {
        budget.check_deadline()?;
        match token {
            Token::Star => {
                next[0] = previous[0];
                for index in 0..subject.len() {
                    check_deadline(index, budget)?;
                    next[index + 1] = previous[index + 1] || next[index];
                }
            }
            Token::Literal(_) | Token::Any | Token::Bracket { .. } => {
                next[0] = false;
                for (index, character) in subject.iter().copied().enumerate() {
                    check_deadline(index, budget)?;
                    next[index + 1] = if previous[index] {
                        token.matches(character, budget)?
                    } else {
                        false
                    };
                }
            }
        }
        std::mem::swap(&mut previous, &mut next);
    }
    budget.check_deadline()?;
    Ok(previous[subject.len()])
}

fn check_deadline(index: usize, budget: &Budget) -> Result<(), CommandFailure> {
    if index.is_multiple_of(256) {
        budget.check_deadline()?;
    }
    Ok(())
}

fn reject_posix(contents: &[PatternChar], budget: &Budget) -> Result<(), CommandFailure> {
    for (index, pair) in contents.windows(2).enumerate() {
        check_deadline(index, budget)?;
        if !pair[0].is_unquoted('[') {
            continue;
        }
        let unsupported = match pair[1] {
            PatternChar::Unquoted(':') => Some(UnsupportedBracket::Class),
            PatternChar::Unquoted('.') => Some(UnsupportedBracket::Collating),
            PatternChar::Unquoted('=') => Some(UnsupportedBracket::Equivalence),
            PatternChar::Unquoted(_) | PatternChar::Literal(_) => None,
        };
        if let Some(unsupported) = unsupported {
            return Err(CommandFailure::usage(unsupported.to_string()));
        }
    }
    Ok(())
}

impl Token<'_> {
    fn matches(&self, character: char, budget: &Budget) -> Result<bool, CommandFailure> {
        Ok(match self {
            Self::Literal(literal) => *literal == character,
            Self::Any | Self::Star => true,
            Self::Bracket { members, negated } => {
                let mut matched = false;
                let mut index = 0;
                let mut visited = 0;
                while index < members.len() {
                    check_deadline(visited, budget)?;
                    visited += 1;
                    if index + 2 < members.len() && members[index + 1].is_unquoted('-') {
                        matched |= (members[index].character()..=members[index + 2].character())
                            .contains(&character);
                        index += 3;
                    } else {
                        matched |= members[index].character() == character;
                        index += 1;
                    }
                }
                matched != *negated
            }
        })
    }
}
