use clap::{Arg, Command};

use super::{CommandFailure, parse_flags};
use crate::builtins::text::lines::parse_count;

pub(crate) const NAME: &str = "xargs";
pub(crate) const HELP: &str = "-I -n";

pub(crate) struct Template {
    words: Vec<String>,
    placeholder: Option<String>,
}

pub(crate) fn parse(arguments: &[String]) -> Result<Template, CommandFailure> {
    let matches = parse_flags(
        Command::new(NAME)
            .arg(
                Arg::new("replace")
                    .short('I')
                    .long("replace")
                    .num_args(1)
                    .allow_hyphen_values(true),
            )
            .arg(Arg::new("count").short('n').num_args(1))
            .arg(
                Arg::new("child")
                    .required(true)
                    .num_args(1..)
                    .trailing_var_arg(true),
            ),
        arguments,
        NAME,
        HELP,
    )?;
    let placeholder = matches.get_one::<String>("replace").cloned();
    if placeholder.as_deref() == Some("") {
        return Err(CommandFailure::usage(
            "xargs: the -I placeholder must not be empty",
        ));
    }
    if let Some(count) = matches.get_one::<String>("count")
        && parse_count(NAME, count)? != 1
    {
        return Err(CommandFailure::usage(
            "xargs: only -n 1 is supported; each element becomes one invocation",
        ));
    }
    let words = matches
        .get_many::<String>("child")
        .expect("required by clap")
        .cloned()
        .collect::<Vec<_>>();
    if let Some(command) = words.first()
        && command.starts_with('-')
    {
        return Err(CommandFailure::usage(format!(
            "xargs: expected a command, found flag {command}"
        )));
    }
    Ok(Template { words, placeholder })
}

impl Template {
    pub(crate) fn expanded_bytes(&self, element: &str) -> Option<u64> {
        let mut total = 0u64;
        for word in &self.words {
            let length = match self.placeholder.as_deref() {
                Some(token) => {
                    let count = u64::try_from(word.matches(token).count()).ok()?;
                    let original = u64::try_from(word.len()).ok()?;
                    let removed = count.checked_mul(u64::try_from(token.len()).ok()?)?;
                    original
                        .checked_sub(removed)?
                        .checked_add(count.checked_mul(u64::try_from(element.len()).ok()?)?)?
                }
                None => u64::try_from(word.len()).ok()?,
            };
            total = total.checked_add(length)?;
        }
        if self.placeholder.is_none() {
            total = total.checked_add(u64::try_from(element.len()).ok()?)?;
        }
        Some(total)
    }

    pub(crate) fn expand(&self, element: &str) -> Vec<String> {
        let mut invocation: Vec<String> = self
            .words
            .iter()
            .map(|word| match self.placeholder.as_deref() {
                Some(token) => word.replace(token, element),
                None => word.clone(),
            })
            .collect();
        if self.placeholder.is_none() {
            invocation.push(element.to_owned());
        }
        invocation
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn replacement_size_is_checked_before_expansion() {
        let words = vec!["-I".into(), "x".into(), "echo".into(), "xx".into()];
        let template = parse(&words).expect("template");
        assert_eq!(template.expanded_bytes("abc"), Some(10));
        assert_eq!(template.expand("abc"), ["echo", "abcabc"]);
    }

    #[test]
    fn child_flags_remain_child_words_after_attached_count() {
        let args = ["-n1", "grep", "-in", "P"].map(str::to_owned);
        let template = parse(&args).expect("child argv");
        assert_eq!(template.expand("needle"), ["grep", "-in", "P", "needle"]);
    }

    #[test]
    fn malformed_usage_is_refused() {
        for words in [
            vec![],
            vec!["-I"],
            vec!["-n", "5", "echo"],
            vec!["-P", "4", "echo"],
        ] {
            assert!(parse(&words.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_err());
        }
    }
}
