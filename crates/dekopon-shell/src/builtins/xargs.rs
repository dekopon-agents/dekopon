use super::CommandFailure;

pub(crate) const NAME: &str = "xargs";
pub(crate) const HELP: &str = "-I -n";

pub(crate) struct Template<'a> {
    words: &'a [String],
    placeholder: Option<&'a str>,
}

pub(crate) fn parse(arguments: &[String]) -> Result<Template<'_>, CommandFailure> {
    let mut placeholder = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "-I" | "--replace" => {
                let Some(value) = arguments.get(index + 1) else {
                    return Err(CommandFailure::usage(
                        "xargs: -I requires a placeholder token",
                    ));
                };
                if value.is_empty() {
                    return Err(CommandFailure::usage(
                        "xargs: the -I placeholder must not be empty",
                    ));
                }
                placeholder = Some(value.as_str());
                index += 2;
            }
            "-n" => {
                let Some(value) = arguments.get(index + 1) else {
                    return Err(CommandFailure::usage("xargs: -n requires a count"));
                };
                if value != "1" {
                    return Err(CommandFailure::usage(
                        "xargs: only -n 1 is supported; each element becomes one invocation",
                    ));
                }
                index += 2;
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(super::unsupported_flag("xargs", flag, HELP));
            }
            _ => break,
        }
    }
    let words = &arguments[index..];
    let Some(command) = words.first() else {
        return Err(CommandFailure::usage(
            "xargs: a command is required, as in `xargs gh issue view`",
        ));
    };
    if command.starts_with('-') {
        return Err(CommandFailure::usage(format!(
            "xargs: expected a command, found flag {command}"
        )));
    }
    Ok(Template { words, placeholder })
}

impl Template<'_> {
    pub(crate) fn expanded_bytes(&self, element: &str) -> Option<u64> {
        let mut total = 0u64;
        for word in self.words {
            let length = match self.placeholder {
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
            .map(|word| match self.placeholder {
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
