use std::{sync::Arc, time::Duration};

use dekopon_model_token_governor::{HistoryRow, Metering, Tokens, UnixMillis};
use serde::Deserialize;
use serde_json::json;
use thiserror::Error;

use crate::metering::RestoreConfig;

const MIN_BUCKET: Duration = Duration::from_secs(60);
const MAX_BUCKETS: u32 = 500;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const TERMS_SIZE: u32 = 1_000;
/// A clock earlier than this has not been synced yet (the Pi has no RTC), so restore would query
/// the wrong window.
const CLOCK_FLOOR: UnixMillis = UnixMillis(1_790_000_000_000);

#[derive(Debug, Error)]
pub(crate) enum RestoreError {
    #[error("the clock reads before this release existed; it has not been synced")]
    ClockUnsynced,
    #[error("restore credential environment variable {variable} is unset or not UTF-8")]
    Credential { variable: String },
    #[error("restore search client could not be built")]
    Client(#[source] reqwest::Error),
    #[error("restore search request failed")]
    Request(#[source] reqwest::Error),
    #[error("restore search answered HTTP {status}")]
    Status { status: u16 },
    #[error("restore search response exceeded {MAX_RESPONSE_BYTES} bytes")]
    TooLarge,
    #[error("restore search response is not the expected JSON")]
    Decode(#[source] serde_json::Error),
    #[error("restore search returned a partial result")]
    Partial,
    #[error("restore search row has an unreadable bucket time")]
    BucketTime,
    #[error("restore search did not answer within its timeout")]
    Timeout,
}

impl RestoreError {
    const fn kind(&self) -> &'static str {
        match self {
            Self::ClockUnsynced => "clock-unsynced",
            Self::Credential { .. } => "credential",
            Self::Client(_) => "client",
            Self::Request(_) => "request",
            Self::Status { .. } => "status",
            Self::TooLarge => "too-large",
            Self::Decode(_) => "decode",
            Self::Partial => "partial",
            Self::BucketTime => "bucket-time",
            Self::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Window {
    pub since: UnixMillis,
    pub until: UnixMillis,
    pub bucket: Duration,
}

impl Window {
    pub(crate) fn at_boot(boot: UnixMillis, lookback: Duration) -> Self {
        Self {
            since: boot.saturating_sub(lookback),
            until: boot,
            bucket: MIN_BUCKET.max(lookback / MAX_BUCKETS),
        }
    }

    const fn since_micros(self) -> i64 {
        self.since.0.saturating_mul(1_000)
    }

    const fn until_micros(self) -> i64 {
        self.until.0.saturating_mul(1_000)
    }
}

pub(crate) fn spawn(
    metering: &Arc<Metering>,
    restore: Option<RestoreConfig>,
    ca_certificate: Option<Vec<u8>>,
    tasks: &mut tokio::task::JoinSet<()>,
) {
    if let Some(restore) = restore.filter(|_| metering.has_budgets()) {
        tasks.spawn(restore_once(
            Arc::clone(metering),
            restore,
            ca_certificate,
            metering.now(),
        ));
    }
}

/// Runs once per boot: waits out the ingest lag, queries the history before boot, and swaps the
/// restored meters in, or keeps the live ones and warns.
pub(crate) async fn restore_once(
    metering: Arc<Metering>,
    config: RestoreConfig,
    ca_certificate: Option<Vec<u8>>,
    boot: UnixMillis,
) {
    let outcome = async {
        if boot < CLOCK_FLOOR {
            return Err(RestoreError::ClockUnsynced);
        }
        tokio::time::sleep(config.delay()).await;
        let lookback = metering.lookback(boot).min(config.lookback_max());
        let window = Window::at_boot(boot, lookback);
        let mut rows =
            tokio::time::timeout(config.timeout(), query(&config, ca_certificate, window))
                .await
                .map_err(|_elapsed| RestoreError::Timeout)??;
        rows.retain(|row| row.at >= window.since && row.at < window.until);
        Ok((window, rows))
    }
    .await;
    match outcome {
        Ok((window, rows)) => {
            metering.restore(window.since, &rows);
            tracing::info!(
                target: "meter",
                event = "meter.restore",
                rows = rows.len(),
                lookback_ms = (window.until.0 - window.since.0).max(0),
                "token windows restored"
            );
        }
        Err(error) => {
            metering.abandon_restore();
            tracing::warn!(
                target: "meter",
                event = "meter.restore",
                error.kind = error.kind(),
                error = %error,
                "token windows start from live charges only"
            );
        }
    }
}

async fn query(
    config: &RestoreConfig,
    ca_certificate: Option<Vec<u8>>,
    window: Window,
) -> Result<Vec<HistoryRow>, RestoreError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(config.timeout());
    if let Some(pem) = ca_certificate {
        for certificate in
            reqwest::Certificate::from_pem_bundle(&pem).map_err(RestoreError::Client)?
        {
            builder = builder.add_root_certificate(certificate);
        }
    }
    let client = builder.build().map_err(RestoreError::Client)?;
    match config {
        RestoreConfig::Openobserve {
            endpoint,
            org,
            stream,
            auth_env,
            ..
        } => {
            let auth = std::env::var(auth_env).map_err(|_unset| RestoreError::Credential {
                variable: auth_env.clone(),
            })?;
            let url = format!(
                "{}/api/{org}/_search?type=logs",
                endpoint.trim_end_matches('/')
            );
            let body = read(
                client
                    .post(url)
                    .header(reqwest::header::AUTHORIZATION, auth)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(openobserve_query(stream, window).to_string()),
            )
            .await?;
            parse_openobserve(&body)
        }
        RestoreConfig::Quickwit {
            endpoint, index, ..
        } => {
            let url = format!("{}/api/v1/{index}/search", endpoint.trim_end_matches('/'));
            let body = read(
                client
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(quickwit_query(window).to_string()),
            )
            .await?;
            parse_quickwit(&body)
        }
    }
}

async fn read(request: reqwest::RequestBuilder) -> Result<Vec<u8>, RestoreError> {
    let mut response = request.send().await.map_err(RestoreError::Request)?;
    let status = response.status();
    if !status.is_success() {
        return Err(RestoreError::Status {
            status: status.as_u16(),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(RestoreError::Request)? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(RestoreError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(crate) fn openobserve_query(stream: &str, window: Window) -> serde_json::Value {
    let sql = format!(
        "SELECT histogram(_timestamp, '{} seconds') AS b, agent, model_name, \
         SUM(usage_input_tokens) AS input, SUM(usage_output_tokens) AS output \
         FROM \"{stream}\" WHERE meter_schema = 1 AND outcome != 'refused' \
         GROUP BY b, agent, model_name ORDER BY b",
        window.bucket.as_secs()
    );
    json!({
        "query": {
            "sql": sql,
            "start_time": window.since_micros(),
            "end_time": window.until_micros(),
            "from": 0,
            "size": -1,
        }
    })
}

pub(crate) fn quickwit_query(window: Window) -> serde_json::Value {
    json!({
        "query": "attributes.meter.schema:1 AND NOT attributes.outcome:refused",
        "max_hits": 0,
        "start_timestamp": window.since.0.div_euclid(1_000),
        "end_timestamp": window.until.0.div_euclid(1_000),
        "aggs": {
            "agent": {
                "terms": {"field": "attributes.agent", "size": TERMS_SIZE},
                "aggs": {
                    "model": {
                        "terms": {"field": "attributes.model.name", "size": TERMS_SIZE},
                        "aggs": {
                            "bucket": {
                                "date_histogram": {
                                    "field": "timestamp_nanos",
                                    "fixed_interval": format!("{}s", window.bucket.as_secs()),
                                    "min_doc_count": 1,
                                },
                                "aggs": {
                                    "input": {"sum": {"field": "attributes.usage.input_tokens"}},
                                    "output": {"sum": {"field": "attributes.usage.output_tokens"}},
                                },
                            },
                        },
                    },
                },
            },
        },
    })
}

#[derive(Deserialize)]
struct OpenObserveResponse {
    #[serde(default)]
    is_partial: bool,
    hits: Vec<OpenObserveRow>,
}

#[derive(Deserialize)]
struct OpenObserveRow {
    b: BucketTime,
    agent: String,
    model_name: String,
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BucketTime {
    Micros(i64),
    Text(String),
}

impl BucketTime {
    fn millis(&self) -> Option<i64> {
        match self {
            Self::Micros(micros) => Some(micros.div_euclid(1_000)),
            Self::Text(text) => utc_millis(text),
        }
    }
}

pub(crate) fn parse_openobserve(body: &[u8]) -> Result<Vec<HistoryRow>, RestoreError> {
    let response =
        serde_json::from_slice::<OpenObserveResponse>(body).map_err(RestoreError::Decode)?;
    if response.is_partial {
        return Err(RestoreError::Partial);
    }
    response
        .hits
        .into_iter()
        .filter_map(|row| {
            let tokens = tokens(row.input).saturating_add(tokens(row.output));
            let Ok(agent) = row.agent.parse() else {
                tracing::debug!(target: "meter", "restore skipped a row naming no valid agent");
                return None;
            };
            Some(
                row.b
                    .millis()
                    .ok_or(RestoreError::BucketTime)
                    .map(|at| HistoryRow {
                        agent,
                        model: row.model_name,
                        at: UnixMillis(at),
                        tokens: Tokens(tokens),
                    }),
            )
        })
        .collect()
}

#[derive(Deserialize)]
struct QuickwitResponse {
    #[serde(default)]
    errors: Vec<serde_json::Value>,
    aggregations: QuickwitAggregations,
}

#[derive(Deserialize)]
struct QuickwitAggregations {
    agent: Terms<AgentBucket>,
}

#[derive(Deserialize)]
struct Terms<B> {
    #[serde(default)]
    sum_other_doc_count: u64,
    buckets: Vec<B>,
}

#[derive(Deserialize)]
struct AgentBucket {
    key: String,
    model: Terms<ModelBucket>,
}

#[derive(Deserialize)]
struct ModelBucket {
    key: String,
    bucket: Histogram,
}

#[derive(Deserialize)]
struct Histogram {
    buckets: Vec<TimeBucket>,
}

#[derive(Deserialize)]
struct TimeBucket {
    key: f64,
    input: Sum,
    output: Sum,
}

#[derive(Deserialize)]
struct Sum {
    value: Option<f64>,
}

pub(crate) fn parse_quickwit(body: &[u8]) -> Result<Vec<HistoryRow>, RestoreError> {
    let response =
        serde_json::from_slice::<QuickwitResponse>(body).map_err(RestoreError::Decode)?;
    let agents = response.aggregations.agent;
    if !response.errors.is_empty()
        || agents.sum_other_doc_count > 0
        || agents
            .buckets
            .iter()
            .any(|agent| agent.model.sum_other_doc_count > 0)
    {
        return Err(RestoreError::Partial);
    }
    let mut rows = Vec::new();
    for agent in agents.buckets {
        let Ok(id) = agent.key.parse() else {
            tracing::debug!(target: "meter", "restore skipped a bucket naming no valid agent");
            continue;
        };
        for model in agent.model.buckets {
            for bucket in model.bucket.buckets {
                if !bucket.key.is_finite() {
                    return Err(RestoreError::BucketTime);
                }
                rows.push(HistoryRow {
                    agent: dekopon_core::AgentId::clone(&id),
                    model: model.key.clone(),
                    at: UnixMillis(bucket.key as i64),
                    tokens: Tokens(
                        tokens(bucket.input.value).saturating_add(tokens(bucket.output.value)),
                    ),
                });
            }
        }
    }
    Ok(rows)
}

/// Both stores sum into f64, which is exact below 2^53.
fn tokens(sum: Option<f64>) -> u64 {
    match sum {
        Some(value) if value.is_finite() && value > 0.0 => value as u64,
        Some(_) | None => 0,
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fraction][Z]`, read as UTC.
fn utc_millis(text: &str) -> Option<i64> {
    let text = text.trim_end_matches('Z');
    let (date, time) = text.split_once(['T', ' '])?;
    let mut date = date.split('-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut clock = clock.split(':').map(str::parse::<i64>);
    let (hour, minute, second) = (
        clock.next()?.ok()?,
        clock.next()?.ok()?,
        clock.next()?.ok()?,
    );
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let millis = fraction
        .get(..fraction.len().min(3))
        .filter(|digits| !digits.is_empty())
        .map_or(Some(0), |digits| {
            digits
                .parse::<i64>()
                .ok()
                .map(|value| value * 10_i64.pow(3 - u32::try_from(digits.len()).unwrap_or(3)))
        })?;
    let days = days_from_civil(year, month, day);
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1_000 + millis)
}

/// Howard Hinnant's `days_from_civil`: <https://howardhinnant.github.io/date_algorithms.html>.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use dekopon_model_token_governor::{Budget, MeterSpec};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::metering::{HumanDuration, LookbackMax, RestoreDelay, RestoreTimeout};

    const HOUR: i64 = 3_600_000;

    fn agent() -> dekopon_core::AgentId {
        "gylmar".parse().unwrap()
    }

    #[test]
    fn openobserve_rows_parse_from_text_or_microsecond_buckets() {
        let rows = parse_openobserve(br#"{"took":12,"hits":[
            {"b":"2026-09-30T10:00:00","agent":"gylmar","model_name":"astra","input":1200,"output":34},
            {"b":1759230300000000,"agent":"gylmar","model_name":"terra","input":10.0,"output":null}
        ],"total":2,"is_partial":false,"scan_size":1}"#)
        .unwrap();
        assert_eq!(
            rows,
            [
                HistoryRow {
                    agent: agent(),
                    model: "astra".to_owned(),
                    at: UnixMillis(1_790_762_400_000),
                    tokens: Tokens(1_234),
                },
                HistoryRow {
                    agent: agent(),
                    model: "terra".to_owned(),
                    at: UnixMillis(1_759_230_300_000),
                    tokens: Tokens(10),
                },
            ]
        );
    }

    #[test]
    fn a_partial_openobserve_result_applies_nothing() {
        assert!(matches!(
            parse_openobserve(br#"{"hits":[{"b":"2026-09-30T10:00:00","agent":"gylmar","model_name":"astra","input":1,"output":1}],"is_partial":true}"#),
            Err(RestoreError::Partial)
        ));
    }

    const QUICKWIT: &str = r#"{"num_hits":3,"hits":[],"elapsed_time_micros":900,"errors":[],
        "aggregations":{"agent":{"doc_count_error_upper_bound":0,"sum_other_doc_count":0,"buckets":[
          {"key":"gylmar","doc_count":3,"model":{"doc_count_error_upper_bound":0,"sum_other_doc_count":0,"buckets":[
            {"key":"astra","doc_count":3,"bucket":{"buckets":[
              {"key":1790762400000.0,"key_as_string":"2026-09-30T10:00:00Z","doc_count":2,"input":{"value":1200.0},"output":{"value":34.0}},
              {"key":1790762700000.0,"key_as_string":"2026-09-30T10:05:00Z","doc_count":1,"input":{"value":5.0},"output":{"value":null}}
            ]}}]}}]}}}"#;

    #[test]
    fn quickwit_buckets_parse_into_rows() {
        let rows = parse_quickwit(QUICKWIT.as_bytes()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].at, UnixMillis(1_790_762_400_000));
        assert_eq!(rows[0].tokens, Tokens(1_234));
        assert_eq!(rows[1].tokens, Tokens(5));
    }

    #[test]
    fn a_truncated_or_failed_quickwit_result_applies_nothing() {
        let truncated = QUICKWIT.replacen(
            r#""sum_other_doc_count":0"#,
            r#""sum_other_doc_count":7"#,
            1,
        );
        assert!(matches!(
            parse_quickwit(truncated.as_bytes()),
            Err(RestoreError::Partial)
        ));
        let failed = QUICKWIT.replace(r#""errors":[]"#, r#""errors":[{"split":"a"}]"#);
        assert!(matches!(
            parse_quickwit(failed.as_bytes()),
            Err(RestoreError::Partial)
        ));
    }

    #[test]
    fn the_window_buckets_at_least_a_minute_and_at_most_five_hundred_per_lookback() {
        let boot = UnixMillis(100 * HOUR);
        assert_eq!(
            Window::at_boot(boot, Duration::from_secs(5 * 3_600)).bucket,
            Duration::from_secs(60)
        );
        let week = Window::at_boot(boot, Duration::from_secs(7 * 86_400));
        assert_eq!(week.bucket, Duration::from_secs(7 * 86_400) / 500);
        assert_eq!(week.since, UnixMillis(100 * HOUR - 7 * 24 * HOUR));
        let sql = openobserve_query("dekopon", week);
        assert_eq!(sql["query"]["start_time"], week.since.0 * 1_000);
        assert!(
            sql["query"]["sql"]
                .as_str()
                .unwrap()
                .contains("'1209 seconds'")
        );
        assert_eq!(
            quickwit_query(week)["aggs"]["agent"]["aggs"]["model"]["aggs"]["bucket"]["date_histogram"]
                ["fixed_interval"],
            "1209s"
        );
    }

    fn metering() -> Arc<Metering> {
        let budget = Budget::new(
            agent(),
            Some(BTreeSet::from(["astra".to_owned()])),
            &[MeterSpec::Rolling {
                limit: Tokens(100_000),
                period: Duration::from_secs(5 * 3_600),
            }],
            UnixMillis::now(),
        );
        let metering = Arc::new(Metering::new(vec![budget], Metering::system_clock()));
        metering.begin_restore();
        metering
    }

    fn used(metering: &Metering) -> u64 {
        metering.statuses(&agent()).unwrap()[0].1.used.0
    }

    fn charge(metering: &Arc<Metering>, tokens: u64) {
        let admission = metering
            .admit(
                dekopon_model_token_governor::Call {
                    agent: agent(),
                    model: "astra".to_owned(),
                    backend: "codex",
                    via: dekopon_model_token_governor::Via::Agent,
                },
                dekopon_model_token_governor::Estimate {
                    input: Tokens(tokens),
                    output_reserve: Tokens(0),
                },
            )
            .unwrap();
        admission.settle(dekopon_model_token_governor::Outcome::Failed);
    }

    fn quickwit(endpoint: String, timeout: Duration) -> RestoreConfig {
        RestoreConfig::Quickwit {
            endpoint,
            index: "otel-logs-v0_9".to_owned(),
            delay: RestoreDelay(HumanDuration(Duration::ZERO)),
            timeout: RestoreTimeout(HumanDuration(timeout)),
            lookback_max: LookbackMax::default(),
        }
    }

    async fn serve_once(body: Option<String>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 64 * 1024];
            let _read = stream.read(&mut request).await.unwrap();
            match body {
                Some(body) => {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
                None => std::future::pending::<()>().await,
            }
        });
        (endpoint, server)
    }

    #[tokio::test]
    async fn a_restore_replays_history_beneath_the_live_tail() {
        let now = UnixMillis::now().0;
        let bucket = now - HOUR;
        let body = QUICKWIT
            .replace("1790762400000.0", &format!("{bucket}.0"))
            .replace("1790762700000.0", &format!("{}.0", bucket + 300_000));
        let (endpoint, server) = serve_once(Some(body)).await;
        let metering = metering();
        charge(&metering, 7);
        restore_once(
            Arc::clone(&metering),
            quickwit(endpoint, Duration::from_secs(5)),
            None,
            UnixMillis(now),
        )
        .await;
        server.await.unwrap();
        assert_eq!(used(&metering), 1_234 + 5 + 7);
    }

    #[tokio::test]
    async fn a_row_outside_the_window_restores_nothing() {
        let now = UnixMillis::now().0;
        let body = QUICKWIT
            .replace("1790762400000.0", &format!("{}.0", now - HOUR))
            .replace("1790762700000.0", "253402300800000000.0");
        let (endpoint, server) = serve_once(Some(body)).await;
        let metering = metering();
        charge(&metering, 7);
        restore_once(
            Arc::clone(&metering),
            quickwit(endpoint, Duration::from_secs(5)),
            None,
            UnixMillis(now),
        )
        .await;
        server.await.unwrap();
        assert_eq!(used(&metering), 1_234 + 7);
    }

    #[tokio::test]
    async fn a_restore_that_times_out_keeps_the_live_meters() {
        let (endpoint, server) = serve_once(None).await;
        let metering = metering();
        charge(&metering, 7);
        restore_once(
            Arc::clone(&metering),
            quickwit(endpoint, Duration::from_millis(50)),
            None,
            UnixMillis::now(),
        )
        .await;
        server.abort();
        assert_eq!(used(&metering), 7);
        charge(&metering, 3);
        assert_eq!(used(&metering), 10);
    }

    #[tokio::test]
    async fn an_unsynced_clock_skips_restore() {
        let metering = metering();
        charge(&metering, 7);
        restore_once(
            Arc::clone(&metering),
            quickwit("http://127.0.0.1:9".to_owned(), Duration::from_millis(50)),
            None,
            UnixMillis(0),
        )
        .await;
        assert_eq!(used(&metering), 7);
    }

    #[tokio::test]
    async fn budgets_without_restore_start_no_restore_task() {
        let metering = Arc::new(Metering::new(Vec::new(), Metering::system_clock()));
        let mut tasks = tokio::task::JoinSet::new();
        spawn(&metering, None, None, &mut tasks);
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn restore_without_budgets_starts_no_restore_task() {
        let metering = Arc::new(Metering::new(Vec::new(), Metering::system_clock()));
        let mut tasks = tokio::task::JoinSet::new();
        spawn(
            &metering,
            Some(quickwit(
                "http://127.0.0.1:9".to_owned(),
                Duration::from_millis(50),
            )),
            None,
            &mut tasks,
        );
        assert!(tasks.is_empty());
    }
}
