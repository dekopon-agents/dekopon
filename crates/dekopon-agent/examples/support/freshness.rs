use dekopon_shell::{CapabilityCallResult, CommandProposal, CommandRun};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const HELP: &str = "Usage: exa search [QUERY] [--input-json JSON] [--start-published-date TIMESTAMP] [--end-published-date TIMESTAMP] [--max-age-hours HOURS] [--text]";
#[cfg(test)]
pub const NOW: &str = "2026-10-31T18:40:00Z";
pub const REFERENCE_PREFIX: &str =
    "[gateway: Dekopon runtime time reference, sampled for this task: ";

pub fn supplied_reference(instruction: &str) -> Option<&str> {
    instruction
        .rsplit_once(REFERENCE_PREFIX)?
        .1
        .split_once(" (UTC).")
        .map(|(reference, _)| reference)
}

pub fn sources() -> Vec<Value> {
    serde_json::from_str(include_str!("../eval/freshness/sources.json"))
        .expect("frozen research sources decode")
}

fn timestamp(text: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(text, &Rfc3339).ok()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    pub start: String,
    pub end: String,
    pub historical: bool,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Problem {
    NoSearch,
    StaleQuery,
    PublicationWindow,
    AnswerFormat,
    Reference,
    UnsupportedClaim,
    MissingDatedEvidence,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Answer {
    reference_utc: Option<String>,
    claims: Vec<Claim>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    source: String,
    fact: String,
    basis: Basis,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Basis {
    Publication,
    UndatedSnapshot,
}

pub fn judge(
    expected: &Expected,
    supplied_reference: Option<&str>,
    calls: &[Value],
    answer: Option<&str>,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    if calls.is_empty() {
        problems.push(Problem::NoSearch);
    }
    let start = timestamp(&expected.start).expect("fixture start timestamp");
    let end = timestamp(&expected.end).expect("fixture end timestamp");
    for call in calls {
        let query = call["query"].as_str().unwrap_or_default().to_lowercase();
        if !expected.historical && (query.contains("june") || query.contains("2026-06")) {
            problems.push(Problem::StaleQuery);
        }
        if call["startPublishedDate"].as_str().and_then(timestamp) != Some(start)
            || call["endPublishedDate"].as_str().and_then(timestamp) != Some(end)
        {
            problems.push(Problem::PublicationWindow);
        }
    }
    let Ok(answer) = serde_json::from_str::<Answer>(answer.unwrap_or_default()) else {
        problems.push(Problem::AnswerFormat);
        return problems;
    };
    if answer.reference_utc.as_deref() != supplied_reference {
        problems.push(Problem::Reference);
    }
    let sources = sources();
    let mut dated = false;
    for claim in answer.claims {
        let Some(source) = sources
            .iter()
            .find(|source| source["id"] == claim.source && source["fact"] == claim.fact)
        else {
            problems.push(Problem::UnsupportedClaim);
            continue;
        };
        let published = source["publishedDate"].as_str().and_then(timestamp);
        match claim.basis {
            Basis::Publication => {
                if published.is_some_and(|at| at >= start && at <= end) {
                    dated = true;
                } else {
                    problems.push(Problem::UnsupportedClaim);
                }
            }
            Basis::UndatedSnapshot => {
                if published.is_some() || expected.historical {
                    problems.push(Problem::UnsupportedClaim);
                }
            }
        }
        if !calls.iter().any(|call| selected(call, source)) {
            problems.push(Problem::UnsupportedClaim);
        }
    }
    if !dated {
        problems.push(Problem::MissingDatedEvidence);
    }
    problems
}

fn selected(input: &Value, source: &Value) -> bool {
    let Some(published) = source["publishedDate"].as_str().and_then(timestamp) else {
        return true;
    };
    input["startPublishedDate"]
        .as_str()
        .and_then(timestamp)
        .is_none_or(|start| published >= start)
        && input["endPublishedDate"]
            .as_str()
            .and_then(timestamp)
            .is_none_or(|end| published <= end)
}

pub struct Research {
    pub calls: Arc<Mutex<Vec<Value>>>,
}

impl Research {
    pub fn parse(&self, argv: &[&str]) -> Result<CommandProposal, CommandRun> {
        let usage = || CommandRun::Rendered {
            stdout: String::new(),
            stderr: "invalid Exa fixture input\n".into(),
            status: 2,
        };
        if argv == ["--help"] || argv == ["search", "--help"] {
            return Err(CommandRun::Rendered {
                stdout: HELP.into(),
                stderr: String::new(),
                status: 0,
            });
        }
        let Some(("search", args)) = argv.split_first().map(|(head, tail)| (*head, tail)) else {
            return Err(usage());
        };
        let mut input = json!({});
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match *arg {
                "--input-json" => {
                    if input != json!({}) {
                        return Err(usage());
                    }
                    input = serde_json::from_str(args.next().ok_or_else(usage)?)
                        .map_err(|error| {
                            tracing::debug!(error.kind = ?error.classify(), "freshness.fixture.invalid-json");
                            usage()
                        })?;
                    if args.next().is_some() {
                        return Err(usage());
                    }
                    break;
                }
                "--start-published-date" | "--end-published-date" => {
                    let key = if *arg == "--start-published-date" {
                        "startPublishedDate"
                    } else {
                        "endPublishedDate"
                    };
                    input[key] = json!(args.next().ok_or_else(usage)?);
                }
                "--max-age-hours" => {
                    let hours: i64 = args
                        .next()
                        .ok_or_else(usage)?
                        .parse()
                        .map_err(|error: std::num::ParseIntError| {
                            tracing::debug!(error.kind = ?error.kind(), "freshness.fixture.invalid-hours");
                            usage()
                        })?;
                    input["contents"]["maxAgeHours"] = json!(hours);
                }
                "--text" => input["contents"]["text"] = json!(true),
                query if !query.starts_with('-') && input.get("query").is_none() => {
                    input["query"] = json!(query)
                }
                _ => return Err(usage()),
            }
        }
        if !input.as_object().is_some_and(|fields| {
            fields.keys().all(|key| {
                [
                    "query",
                    "startPublishedDate",
                    "endPublishedDate",
                    "contents",
                ]
                .contains(&key.as_str())
            })
        }) {
            return Err(usage());
        }
        if input["startPublishedDate"]
            .as_str()
            .and_then(timestamp)
            .zip(input["endPublishedDate"].as_str().and_then(timestamp))
            .is_some_and(|(start, end)| start > end)
        {
            return Err(usage());
        }
        if !input["query"]
            .as_str()
            .is_some_and(|query| !query.trim().is_empty())
            || ["startPublishedDate", "endPublishedDate"]
                .iter()
                .any(|key| {
                    input
                        .get(key)
                        .is_some_and(|v| v.as_str().and_then(timestamp).is_none())
                })
            || input.get("contents").is_some_and(|contents| {
                !contents.is_null()
                    && !contents.as_object().is_some_and(|options| {
                        options.iter().all(|(name, value)| match name.as_str() {
                            "maxAgeHours" => {
                                value.is_null()
                                    || value
                                        .as_i64()
                                        .is_some_and(|hours| (-1..=720).contains(&hours))
                            }
                            "text" => value.is_null() || value.is_boolean(),
                            _ => false,
                        })
                    })
            })
        {
            return Err(usage());
        }
        Ok(CommandProposal {
            capability: "exa.search".into(),
            input,
            secret_use: None,
            report: None,
        })
    }

    pub fn answer(&self, input: &Value) -> Result<Value, CapabilityCallResult> {
        self.calls.lock().push(input.clone());
        Ok(
            json!({"results":sources().into_iter().filter(|source| selected(input, source)).collect::<Vec<_>>()}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> Expected {
        Expected {
            start: "2026-10-28T18:40:00Z".into(),
            end: NOW.into(),
            historical: false,
        }
    }

    fn call() -> Value {
        json!({"query":"AI developer announcements", "startPublishedDate":"2026-10-28T18:40:00Z", "endPublishedDate":NOW})
    }

    fn ledger(claims: Value) -> String {
        json!({"referenceUtc":NOW,"claims":claims}).to_string()
    }

    #[test]
    fn publication_scoring_refuses_cache_age_and_a_freshly_retrieved_june_article() {
        let answer = ledger(json!([{"source":"june","fact":"june-release","basis":"publication"}]));
        let problems = judge(
            &window(),
            Some(NOW),
            &[json!({"query":"announcements", "contents":{"maxAgeHours":72}})],
            Some(&answer),
        );
        assert!(problems.contains(&Problem::PublicationWindow));
        assert!(problems.contains(&Problem::UnsupportedClaim));
        assert!(problems.contains(&Problem::MissingDatedEvidence));
    }

    #[test]
    fn undated_snapshots_and_upcoming_events_keep_their_distinct_date_bases() {
        let answer = ledger(json!([
            {"source":"event","fact":"upcoming-event","basis":"publication"},
            {"source":"plans","fact":"plan-current","basis":"undated-snapshot"}
        ]));
        assert_eq!(judge(&window(), Some(NOW), &[call()], Some(&answer)), []);
        let wrong = ledger(json!([{"source":"plans","fact":"plan-current","basis":"publication"}]));
        assert!(
            judge(&window(), Some(NOW), &[call()], Some(&wrong))
                .contains(&Problem::UnsupportedClaim)
        );
    }

    #[test]
    fn the_executed_query_is_scored_even_when_the_final_claim_is_current() {
        let mut input = call();
        input["query"] = json!("AI developer announcements June 2026");
        let answer = ledger(json!([{"source":"october","fact":"model-new","basis":"publication"}]));
        assert_eq!(
            judge(&window(), Some(NOW), &[input], Some(&answer)),
            [Problem::StaleQuery]
        );
    }

    #[test]
    fn a_supported_baseline_with_no_supplied_clock_accepts_a_null_reference() {
        let answer = json!({"referenceUtc":null,"claims":[
            {"source":"october","fact":"model-new","basis":"publication"}
        ]})
        .to_string();
        assert_eq!(judge(&window(), None, &[call()], Some(&answer)), []);
    }

    #[test]
    fn a_supplied_reference_is_required_and_is_not_replaced_by_the_fixture_clock() {
        let reference = "2026-11-01T00:00:00Z";
        let claims = json!([{"source":"october","fact":"model-new","basis":"publication"}]);
        let answer = json!({"referenceUtc":reference,"claims":claims}).to_string();
        assert_eq!(
            judge(&window(), Some(reference), &[call()], Some(&answer)),
            []
        );
        for reported in [None, Some(NOW)] {
            let answer = json!({"referenceUtc":reported,"claims":claims}).to_string();
            assert_eq!(
                judge(&window(), Some(reference), &[call()], Some(&answer)),
                [Problem::Reference]
            );
        }
    }

    #[test]
    fn historical_scope_does_not_require_guessing_an_unprovided_current_time() {
        let expected = Expected {
            start: "2026-06-01T00:00:00Z".into(),
            end: "2026-06-30T23:59:59Z".into(),
            historical: true,
        };
        let call = json!({"query":"AI developer announcements June 2026",
            "startPublishedDate":expected.start,"endPublishedDate":expected.end});
        let answer = json!({"referenceUtc":null,"claims":[
            {"source":"june","fact":"june-release","basis":"publication"}
        ]})
        .to_string();
        assert_eq!(judge(&expected, None, &[call], Some(&answer)), []);
    }

    #[test]
    fn the_canned_service_rejects_malformed_and_unknown_contents_options() {
        let research = Research {
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        for contents in [
            json!({"maxAgeHours":"72"}),
            json!({"maxAgeHours":72.0}),
            json!({"maxAgeHours":true}),
            json!({"maxAgeHours":72,"unknown":true}),
            json!({"text":"true"}),
            json!("contents"),
            json!([]),
        ] {
            let input = json!({"query":"announcements","contents":contents}).to_string();
            assert!(
                matches!(
                    research.parse(&["search", "--input-json", &input]),
                    Err(CommandRun::Rendered { status: 2, .. })
                ),
                "{input}"
            );
        }
        for contents in [
            json!({"maxAgeHours":72,"text":true}),
            json!({"maxAgeHours":null,"text":null}),
            json!(null),
        ] {
            let input = json!({"query":"announcements","contents":contents});
            let proposal = research
                .parse(&["search", "--input-json", &input.to_string()])
                .unwrap();
            assert_eq!(proposal.input, input);
        }
    }

    #[test]
    fn the_canned_service_rejects_date_only_publication_bounds_before_execution() {
        let research = Research {
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        assert!(matches!(
            research.parse(&[
                "search",
                "announcements",
                "--start-published-date",
                "2026-06-01"
            ]),
            Err(CommandRun::Rendered { status: 2, .. })
        ));
        assert!(research.calls.lock().is_empty());
        let proposal = research
            .parse(&[
                "search",
                "announcements",
                "--start-published-date",
                "2026-06-01T00:00:00Z",
            ])
            .unwrap();
        assert_eq!(proposal.input["startPublishedDate"], "2026-06-01T00:00:00Z");
    }
}
