use std::collections::HashSet;

use dekopon_core::github::{GithubNumber, GithubThread};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PullRequestLinkTarget {
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) number: GithubNumber,
}

#[derive(Clone, Debug)]
pub(crate) struct PullRequestLinkContext {
    pub(crate) targets: Vec<PullRequestLinkTarget>,
}

impl PullRequestLinkContext {
    pub(crate) fn parse(text: &str) -> Option<Self> {
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
            targets.extend(
                bare.split(delimiter)
                    .filter_map(PullRequestLinkTarget::parse),
            );
            let Some((link, tail)) = slack.split_once('>') else {
                break;
            };
            let url = link.split_once('|').map_or(link, |(url, _)| url);
            if let Some(target) = PullRequestLinkTarget::parse(url) {
                targets.push(target);
            }
            remaining = tail;
        }
        let mut seen = HashSet::new();
        targets.retain(|target| {
            seen.insert((
                target.owner.clone(),
                target.repo.clone(),
                target.number.to_string(),
            ))
        });
        (!targets.is_empty()).then_some(Self { targets })
    }

    pub(crate) fn prompt(&self) -> String {
        let targets = self
            .targets
            .iter()
            .map(|target| {
                format!(
                    r#"{{"owner":"{}","repo":"{}","number":{}}}"#,
                    target.owner, target.repo, target.number
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
            .map(|target| target.owner.as_str())
            .collect();
        let names: Vec<_> = self
            .targets
            .iter()
            .map(|target| target.repo.as_str())
            .collect();
        dekopon_telemetry::with_event_string_array("github.repository.owners", &owners, || {
            dekopon_telemetry::with_event_string_array("github.repository.names", &names, emit)
        })
    }
}

impl PullRequestLinkTarget {
    fn parse(candidate: &str) -> Option<Self> {
        let candidate = candidate.trim_end_matches([',', '.', ';', ':', '!', '?']);
        if !candidate.get(..8)?.eq_ignore_ascii_case("https://") || candidate.contains('\\') {
            return None;
        }
        let url = reqwest::Url::parse(candidate).ok()?;
        if url.scheme() != "https"
            || url.host_str() != Some("github.com")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
        {
            return None;
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
            _ => return None,
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
            return None;
        }
        let number = match number.parse::<GithubThread>().ok()? {
            GithubThread::Number(number) => number,
            GithubThread::Repository => return None,
        };
        Some(Self {
            owner: owner.to_ascii_lowercase(),
            repo: repo.to_ascii_lowercase(),
            number,
        })
    }
}
