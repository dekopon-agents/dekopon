use crate::builtins::CommandFailure;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Selection {
    ranges: Vec<(usize, Option<usize>)>,
}

impl Selection {
    pub(crate) fn parse(command: &str, list: &str) -> Result<Self, CommandFailure> {
        let mut ranges = Vec::new();
        for entry in list.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                return Err(CommandFailure::usage(format!(
                    "{command}: empty entry in selection list {list:?}"
                )));
            }
            let parsed = match entry.split_once('-') {
                None => {
                    let position = parse_position(command, entry)?;
                    (position, Some(position))
                }
                Some(("", end)) => (1, Some(parse_position(command, end)?)),
                Some((start, "")) => (parse_position(command, start)?, None),
                Some((start, end)) => (
                    parse_position(command, start)?,
                    Some(parse_position(command, end)?),
                ),
            };
            if let (start, Some(end)) = parsed
                && start > end
            {
                return Err(CommandFailure::usage(format!(
                    "{command}: selection {entry:?} ends before it starts"
                )));
            }
            ranges.push(parsed);
        }
        ranges.sort_unstable_by_key(|(start, _)| *start);
        let mut merged: Vec<(usize, Option<usize>)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            if let Some((_, previous_end)) = merged.last_mut()
                && previous_end.is_none_or(|last| start <= last.saturating_add(1))
            {
                *previous_end = match (*previous_end, end) {
                    (None, _) | (_, None) => None,
                    (Some(left), Some(right)) => Some(left.max(right)),
                };
            } else {
                merged.push((start, end));
            }
        }
        Ok(Self { ranges: merged })
    }

    pub(crate) fn cursor(&self) -> SelectionCursor<'_> {
        SelectionCursor {
            ranges: &self.ranges,
            next: 0,
        }
    }
}

pub(crate) struct SelectionCursor<'a> {
    ranges: &'a [(usize, Option<usize>)],
    next: usize,
}

impl SelectionCursor<'_> {
    pub(crate) fn includes(&mut self, one_based: usize) -> bool {
        while let Some((_, Some(end))) = self.ranges.get(self.next)
            && one_based > *end
        {
            self.next += 1;
        }
        self.ranges.get(self.next).is_some_and(|(start, end)| {
            one_based >= *start && end.is_none_or(|end| one_based <= end)
        })
    }
}

#[allow(
    clippy::map_err_ignore,
    reason = "ParseIntError separates only empty, non-digit, and overflow for a field spec the \
              message quotes back in full; the zero check below is what distinguishes the one \
              rejection an operator is likely to hit"
)]
fn parse_position(command: &str, text: &str) -> Result<usize, CommandFailure> {
    let position = text.trim().parse::<usize>().map_err(|_| {
        CommandFailure::usage(format!("{command}: {text:?} is not a positive position"))
    })?;
    if position == 0 {
        return Err(CommandFailure::usage(format!(
            "{command}: positions are one-based, so 0 is not valid"
        )));
    }
    Ok(position)
}

#[cfg(test)]
mod tests {
    use super::Selection;

    #[test]
    fn unordered_overlapping_ranges_merge_and_a_cursor_advances_once() {
        let selection = Selection::parse("cut", "9-,3-5,1-2,4-8").expect("valid ranges");
        assert_eq!(selection.ranges, vec![(1, None)]);
        let mut cursor = selection.cursor();
        for position in 1..1000 {
            assert!(cursor.includes(position));
        }
        let selection = Selection::parse("cut", "8,2,5-6").expect("valid gaps");
        let mut cursor = selection.cursor();
        let selected = (1..=9)
            .filter(|position| cursor.includes(*position))
            .collect::<Vec<_>>();
        assert_eq!(selected, vec![2, 5, 6, 8]);
    }

    #[test]
    fn malformed_selections_are_rejected() {
        for list in ["0", "3-1", "x", "1,,2", ""] {
            assert!(
                Selection::parse("cut", list).is_err(),
                "{list:?} must be rejected"
            );
        }
    }
}
