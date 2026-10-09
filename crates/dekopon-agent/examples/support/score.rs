use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    #[serde(default)]
    all_of: Vec<String>,
    #[serde(default)]
    none_of: Vec<String>,
    #[serde(default)]
    any_of: Vec<Vec<String>>,
    #[serde(default)]
    script_all_of: Vec<String>,
    #[serde(default)]
    script_none_of: Vec<String>,
    #[serde(default)]
    script_count_max: BTreeMap<String, usize>,
    #[serde(default)]
    output_count: BTreeMap<String, (usize, usize)>,
    #[serde(default)]
    read_before: Vec<(String, String)>,
    require_clean: Option<bool>,
    max_scripts: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rules {
    Basic,
    Orchard,
}

pub struct Attempt<'a> {
    pub fatal: Option<&'a str>,
    pub answer: Option<&'a str>,
    pub scripts: &'a [ScriptView<'a>],
}

pub struct ScriptView<'a> {
    pub script: &'a str,
    pub exit_code: u8,
    pub output_head: &'a str,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "problem", rename_all = "kebab-case")]
pub enum Problem {
    Fatal { kind: String },
    AnswerMissing { needle: String },
    AnswerForbidden { needle: String },
    AnswerAnyOfMissing { needles: Vec<String> },
    ScriptMissing { needle: String },
    ScriptForbidden { needle: String },
    ScriptCount { needle: String, most: usize },
    OutputCount { needle: String, seen: usize },
    Order { script: String, output: String },
    NoCleanScript,
    TooManyScripts { most: usize },
}

pub fn judge(rules: Rules, expected: &Expected, attempt: &Attempt<'_>) -> Vec<Problem> {
    let answer = attempt.answer.unwrap_or("").to_lowercase();
    let lowered = attempt
        .scripts
        .iter()
        .map(|script| script.script)
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    let mut problems = Vec::new();
    if let Some(kind) = attempt.fatal {
        problems.push(Problem::Fatal {
            kind: kind.to_owned(),
        });
    }
    for needle in &expected.all_of {
        if !answer.contains(&needle.to_lowercase()) {
            problems.push(Problem::AnswerMissing {
                needle: needle.clone(),
            });
        }
    }
    for needle in &expected.none_of {
        if answer.contains(&needle.to_lowercase()) {
            problems.push(Problem::AnswerForbidden {
                needle: needle.clone(),
            });
        }
    }
    for group in &expected.any_of {
        if !group
            .iter()
            .any(|needle| answer.contains(&needle.to_lowercase()))
        {
            problems.push(Problem::AnswerAnyOfMissing {
                needles: group.clone(),
            });
        }
    }
    for needle in &expected.script_all_of {
        if !lowered.contains(&needle.to_lowercase()) {
            problems.push(Problem::ScriptMissing {
                needle: needle.clone(),
            });
        }
    }
    for needle in &expected.script_none_of {
        if lowered.contains(&needle.to_lowercase()) {
            problems.push(Problem::ScriptForbidden {
                needle: needle.clone(),
            });
        }
    }
    for (needle, &most) in &expected.script_count_max {
        if lowered.matches(needle.to_lowercase().as_str()).count() > most {
            problems.push(Problem::ScriptCount {
                needle: needle.clone(),
                most,
            });
        }
    }
    for (needle, &(least, most)) in &expected.output_count {
        let seen = attempt
            .scripts
            .iter()
            .map(|script| script.output_head.matches(needle.as_str()).count())
            .sum();
        if !(least..=most).contains(&seen) {
            problems.push(Problem::OutputCount {
                needle: needle.clone(),
                seen,
            });
        }
    }
    for (script_needle, output_needle) in &expected.read_before {
        let script_needle_lowered = script_needle.to_lowercase();
        let read = attempt.scripts.iter().position(|script| {
            script
                .script
                .to_lowercase()
                .contains(&script_needle_lowered)
        });
        let wrote = attempt
            .scripts
            .iter()
            .position(|script| script.output_head.contains(output_needle.as_str()));
        if let Some(wrote) = wrote
            && read.is_none_or(|read| read > wrote)
        {
            problems.push(Problem::Order {
                script: script_needle.clone(),
                output: output_needle.clone(),
            });
        }
    }
    let require_clean = expected.require_clean.unwrap_or(match rules {
        Rules::Basic => true,
        Rules::Orchard => false,
    });
    if require_clean && !attempt.scripts.iter().any(|script| script.exit_code == 0) {
        problems.push(Problem::NoCleanScript);
    }
    if let Some(most) = expected.max_scripts
        && attempt.scripts.len() > most
    {
        problems.push(Problem::TooManyScripts { most });
    }
    problems
}
