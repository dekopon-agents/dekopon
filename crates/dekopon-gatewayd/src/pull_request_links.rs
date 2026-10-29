use std::collections::HashSet;

use dekopon_core::{
    ExternalSubject, SubjectError,
    github::{GithubNumber, GithubThread, RepoFullName},
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullRequestLinkTarget {
    pub(crate) repository: RepoFullName,
    pub(crate) number: GithubNumber,
}

#[derive(Clone, Debug)]
pub(crate) struct PullRequestLinkContext {
    pub(crate) targets: Vec<PullRequestLinkTarget>,
}

impl PullRequestLinkContext {
    pub(crate) fn parse(text: &str) -> Result<Option<Self>, SubjectError> {
        let delimiter = |c: char| c.is_whitespace() || "<>|\"[]()`".contains(c);
        let text = if text.len() > crate::transport::MAX_INBOUND_TEXT_BYTES {
            let prefix = &text[..crate::transport::floor_boundary(
                text,
                crate::transport::MAX_INBOUND_TEXT_BYTES,
            )];
            let end = prefix
                .char_indices()
                .rev()
                .find(|(_, c)| delimiter(*c))
                .map_or(0, |(index, c)| index + c.len_utf8());
            &prefix[..end]
        } else {
            text
        };
        let mut remaining = text;
        let mut targets = Vec::new();
        loop {
            let (bare, slack) = remaining.split_once('<').unwrap_or((remaining, ""));
            for candidate in bare.split(delimiter) {
                if let Some(target) = PullRequestLinkTarget::parse(candidate)? {
                    targets.push(target);
                }
            }
            let Some((link, tail)) = slack.split_once('>') else {
                break;
            };
            let url = link.split_once('|').map_or(link, |(url, _)| url);
            if let Some(target) = PullRequestLinkTarget::parse(url)? {
                targets.push(target);
            }
            remaining = tail;
        }
        let mut seen = HashSet::new();
        targets
            .retain(|target| seen.insert((target.repository.clone(), target.number.to_string())));
        Ok((!targets.is_empty()).then_some(Self { targets }))
    }

    pub(crate) fn prompt(&self) -> String {
        let targets = self
            .targets
            .iter()
            .map(|target| {
                let (owner, repo) = target.coordinates();
                format!(
                    r#"{{"owner":"{}","repo":"{}","number":{}}}"#,
                    owner, repo, target.number
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "[gateway: pull-request-link trigger] This turn was started by a pull-request link. Review each structured target using granted tools, submit only a head-pinned COMMENT review, follow this thread using the granted follow command (follow stay), and reply with a short summary and the returned review link. Only the poster's leg is available throughout this turn; other speakers wait for separate turns. Targets: [{targets}]"
        )
    }

    pub(crate) fn record<T>(&self, emit: impl FnOnce() -> T) -> T {
        let owners: Vec<_> = self
            .targets
            .iter()
            .map(|target| target.coordinates().0)
            .collect();
        let names: Vec<_> = self
            .targets
            .iter()
            .map(|target| target.coordinates().1)
            .collect();
        dekopon_telemetry::with_event_string_array("github.repository.owners", &owners, || {
            dekopon_telemetry::with_event_string_array("github.repository.names", &names, emit)
        })
    }
}

impl PullRequestLinkTarget {
    pub(crate) fn coordinates(&self) -> (&str, &str) {
        self.repository.as_str().split_once('/').unwrap_or_default()
    }

    fn parse(candidate: &str) -> Result<Option<Self>, SubjectError> {
        let candidate = candidate.trim_end_matches([',', '.', ';', ':', '!', '?']);
        if !candidate
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
            || candidate.contains('\\')
        {
            return Ok(None);
        }
        let Ok(url) = reqwest::Url::parse(candidate) else {
            return Ok(None);
        };
        if url.scheme() != "https"
            || url.host_str() != Some("github.com")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
        {
            return Ok(None);
        }
        let segments: Vec<_> = url
            .path()
            .strip_suffix('/')
            .unwrap_or(url.path())
            .split('/')
            .skip(1)
            .collect();
        let (owner, repo, number) = match segments.as_slice() {
            [owner, repo, "pull", number]
            | [owner, repo, "pull", number, "files" | "commits" | "checks"] => {
                (*owner, *repo, *number)
            }
            _ => return Ok(None),
        };
        if owner.is_empty()
            || repo.is_empty()
            || repo == "."
            || repo == ".."
            || !owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || !repo
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Ok(None);
        }
        let subject = ExternalSubject::github(owner, repo)?;
        let repository = RepoFullName::new(&format!(
            "{}/{}",
            subject.tenant().unwrap_or_default(),
            subject.subject()
        ))
        .map_err(|_| SubjectError::InvalidSegment {
            segment: "subject",
            value: repo.to_owned(),
        })?;
        let Ok(number) = number.parse::<GithubThread>() else {
            return Ok(None);
        };
        let number = match number {
            GithubThread::Number(number) => number,
            GithubThread::Repository => return Ok(None),
        };
        Ok(Some(Self { repository, number }))
    }
}
