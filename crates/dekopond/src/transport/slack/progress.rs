//! Ordinary bot-owned progress posts. API contracts: chat.postMessage, chat.update (as_user=true),
//! chat.delete; <https://docs.slack.dev/reference/methods/>. No ephemeral/stream/animation API.
use super::*;
use dekopon_harness::activity::ActivityLabel;
use tokio::time::Instant;

/// Longest a channel-creating post waits for a slot *it observed*, before it gives up.
///
/// A 429 penalty parks the slot for *every* sender in that channel, so this ceiling is what makes
/// an unrelated session's answer wait rather than fail instantly. Nothing parks the slot further
/// than this into the future, so a slot beyond it was never one this caller could reach.
///
/// It is deliberately measured from the observation and not from the caller's entry: a sender
/// already queued behind another when the 429 lands would otherwise be refused for somebody
/// else's backoff after a wait of its own that never approached the ceiling.
const POST_WAIT_CEILING: Duration = Duration::from_secs(60);
/// Longest a channel-creating post waits in total, however many parks land while it is queued.
///
/// The per-observation ceiling above bounds one park. This bounds the caller: a channel whose
/// senders keep being rate limited must not hold a session task forever, so a sender waits out
/// the queue delay it had already spent plus one full park, and no more.
const POST_TOTAL_WAIT_CEILING: Duration = Duration::from_secs(120);
/// What a 429 whose `Retry-After` is absent or unparsable parks the channel slot for.
const UNSTATED_RETRY_PENALTY: Duration = Duration::from_secs(5);
/// Longest server-stated backoff this process sleeps through before retrying the same post once.
const MAX_HONORED_RETRY: Duration = Duration::from_secs(5);
/// What a 429 with no usable `Retry-After` puts on the cosmetic budget's own cooldown.
const UNSTATED_COSMETIC_COOLDOWN: Duration = Duration::from_secs(60);

/// The `Retry-After` a 429 stated, in seconds, or `None` when it stated none this process can use.
fn stated_retry_after(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.clamp(1, 86400)))
}

/// How long a 429 parks the *shared channel slot*, whichever writer of [`PostRate`] observed it.
///
/// One definition for both, because both create a message in the same physical channel and draw
/// the same platform 429: the final `chat.postMessage`/`files.completeUploadExternal` path and the
/// cosmetic ⌛ progress post. A park is what every *other* sender in the channel then waits out, so
/// a day-long `Retry-After` parked here would leave a slot no later caller can reach and turn every
/// answer in that channel into an instant `post-capacity`. The stating caller's own cooldown and
/// refusal keep the full value; only the slot everyone shares is capped.
fn channel_post_park(stated: Option<Duration>) -> Duration {
    stated
        .unwrap_or(UNSTATED_RETRY_PENALTY)
        .min(POST_WAIT_CEILING)
}

/// Only a validated creation response can construct this handle. Not a delivery receipt.
pub(crate) struct OwnedProgressArtifact {
    channel: String,
    timestamp: String,
}

#[cfg(test)]
impl OwnedProgressArtifact {
    /// A handle a test can hold without a live Slack creation response behind it.
    pub(crate) fn fixture(channel: &str, timestamp: &str) -> Self {
        Self {
            channel: channel.to_owned(),
            timestamp: timestamp.to_owned(),
        }
    }
}

#[derive(Default)]
pub(super) struct CosmeticRate {
    requests: VecDeque<Instant>,
    cooldown: Option<Instant>,
}
impl CosmeticRate {
    fn reserve(&mut self, now: Instant) -> Result<(), TransportError> {
        while self
            .requests
            .front()
            .is_some_and(|t| *t + Duration::from_secs(60) <= now)
        {
            self.requests.pop_front();
        }
        let limit = self
            .requests
            .front()
            .filter(|_| self.requests.len() >= 30)
            .map(|t| *t + Duration::from_secs(60));
        if let Some(until) = limit
            .into_iter()
            .chain(self.cooldown)
            .max()
            .filter(|t| *t > now)
        {
            return Err(TransportError::ActivityRateLimited {
                retry_after: until - now,
            });
        }
        self.requests.push_back(now);
        Ok(())
    }
}
/// Shared physical channel post slots; final reservations outrank any new cosmetic post.
#[derive(Default)]
pub(super) struct PostRate {
    next: HashMap<String, Instant>,
}
impl PostRate {
    fn reserve(&mut self, channel: &str, final_post: bool) -> Result<Instant, TransportError> {
        let now = Instant::now();
        self.next.retain(|_, next| *next > now);
        let slot = self.next.get(channel).copied().unwrap_or(now);
        if (!final_post && slot > now)
            || (self.next.len() >= 128 && !self.next.contains_key(channel))
        {
            return Err(TransportError::ActivityRateLimited {
                retry_after: Duration::from_secs(1),
            });
        }
        // Claim only a transmission that can start now. Future reservations can become stale
        // when a prior request reaches Slack later than it was dispatched locally.
        if slot <= now {
            self.next
                .insert(channel.to_owned(), now + Duration::from_secs(1));
        }
        Ok(slot)
    }

    fn completed(&mut self, channel: &str, interval: Duration) {
        let next = self
            .next
            .entry(channel.to_owned())
            .or_insert_with(Instant::now);
        *next = (*next).max(Instant::now() + interval);
    }
}
fn body(label: ActivityLabel) -> Value {
    // Detail is plain_text with emoji parsing off. Fallback is fixed, non-linking and cannot
    // mention anyone, even when operator text resembles Slack's proprietary markup.
    // Escape even in plain_text and re-bound after expansion, without cutting an entity or UTF-8.
    let mut detail = String::from("⌛ ");
    for c in label.as_str().chars() {
        let escaped = match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            _ => c.to_string(),
        };
        if detail.len() + escaped.len() > 84 {
            break;
        }
        detail.push_str(&escaped);
    }
    json!({"text":"Working…", "mrkdwn":false, "parse":"none", "link_names":false,
        "unfurl_links":false, "unfurl_media":false,
        "blocks":[{"type":"section", "text":{"type":"plain_text", "text":detail, "emoji":false}}]})
}
impl SlackReplier {
    /// Waits for this physical channel's next post slot, refusing only a wait it cannot afford.
    ///
    /// A channel parked by somebody else's 429 makes this caller *wait*; it never turns an
    /// unrelated session's paid-for answer into an instant `post-capacity`. The affordable wait is
    /// recomputed against each observation ([`POST_WAIT_CEILING`]) rather than frozen at `entered`,
    /// because a park that lands while this caller is queued is dated from the 429's arrival, not
    /// from this caller's entry; `entered` bounds only the total ([`POST_TOTAL_WAIT_CEILING`]).
    async fn reserve_channel_post(
        &self,
        channel: &str,
        entered: Instant,
    ) -> Result<(), TransportError> {
        loop {
            let reservation = self
                .post_rate
                .lock()
                .expect("Slack channel post rate")
                .reserve(channel, true);
            let limit = (Instant::now() + POST_WAIT_CEILING).min(entered + POST_TOTAL_WAIT_CEILING);
            match reservation {
                Ok(slot) if slot > limit => {
                    return Err(TransportError::Service {
                        code: "post-capacity".into(),
                    });
                }
                // Waiting senders recheck the slot at transmission time, so a reservation taken
                // while this one slept is never reused.
                Ok(slot) if slot > Instant::now() => tokio::time::sleep_until(slot).await,
                Ok(_) => return Ok(()),
                Err(TransportError::ActivityRateLimited { retry_after })
                    if Instant::now() + retry_after <= limit =>
                {
                    tokio::time::sleep(retry_after).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Extends this channel's slot after a transmission completed or a 429 penalty was honored.
    fn channel_post_completed(&self, channel: &str, interval: Duration) {
        self.post_rate
            .lock()
            .expect("Slack channel post rate")
            .completed(channel, interval);
    }

    /// One channel-creating POST through the shared per-channel slot.
    ///
    /// `chat.postMessage` and `files.completeUploadExternal` both create a message in the channel,
    /// so both reserve the same slot, extend it identically, and honor a 429 the same way. Grep
    /// for the platform endpoints rather than for this helper when adding a third.
    pub(super) async fn paced_channel_post(
        &self,
        method: &str,
        body: &Value,
        channel: &str,
    ) -> Result<Value, TransportError> {
        let entered = Instant::now();
        for attempt in 0..2 {
            self.reserve_channel_post(channel, entered).await?;
            let response = self
                .http
                .post(format!("{}/api/{method}", self.endpoint))
                .header(
                    "authorization",
                    format!("Bearer {}", self.bot_token.expose()),
                )
                .header("content-type", "application/json; charset=utf-8")
                .body(serde_json::to_vec(body).expect("JSON value serializes"))
                .send()
                .await
                .map_err(|source| TransportError::Request(Box::new(source)));
            self.channel_post_completed(channel, Duration::from_secs(1));
            let response = response?;
            // Only an explicit HTTP 429 proves nonacceptance. No EOF, timeout, malformed success,
            // or other uncertain message creation is ever retried.
            if response.status().as_u16() != 429 {
                return check_ok(response).await;
            }
            let stated = stated_retry_after(&response);
            // The penalty is bounded by the wait ceiling: a day-long Retry-After must not make
            // every other sender in this channel wait past it, and it must not park the slot
            // somewhere no later caller can reach.
            let penalty = channel_post_park(stated);
            self.channel_post_completed(channel, penalty);
            // One retry per post, and only for a backoff the service actually stated and this
            // caller can sit through. Anything longer is this caller's refusal, not the channel's.
            let honored = stated
                .filter(|_| attempt == 0)
                .filter(|stated| *stated <= MAX_HONORED_RETRY)
                .filter(|stated| Instant::now() + *stated <= entered + POST_TOTAL_WAIT_CEILING);
            let Some(honored) = honored else {
                tracing::warn!(
                    event = "gateway_reply_rate_limited",
                    transport = "slack",
                    method,
                    retry_after_seconds = penalty.as_secs()
                );
                return Err(TransportError::Service {
                    code: "ratelimited".into(),
                });
            };
            tokio::time::sleep(honored).await;
        }
        Err(TransportError::Service {
            code: "ratelimited".into(),
        })
    }

    pub(super) async fn post_answer(
        &self,
        body: &Value,
        channel: &str,
    ) -> Result<Value, TransportError> {
        self.paced_channel_post("chat.postMessage", body, channel)
            .await
    }
    pub(super) async fn create_progress(
        &self,
        target: ActivityTarget,
        label: ActivityLabel,
    ) -> Result<Option<OwnedProgressArtifact>, TransportError> {
        if !self.progress_available.load(Ordering::Acquire) {
            return Ok(None);
        }
        let ActivityTarget::Slack {
            channel_id,
            thread_ts,
            message_ts,
            ..
        } = target
        else {
            return Err(TransportError::Response);
        };
        let mut body = body(label);
        body["channel"] = json!(channel_id);
        // Same authenticated destination as replies, including classic whole-DM behavior.
        if self.experience == SlackExperience::Agent || !channel_id.starts_with(['D', 'd']) {
            body["thread_ts"] = json!(thread_ts);
        }
        body["reply_broadcast"] = json!(false);
        let response = self.cosmetic_json("chat.postMessage", &body).await?;
        let timestamp = response["ts"].as_str().ok_or(TransportError::Response)?;
        if response["channel"] != channel_id
            || !canonical_timestamp(timestamp)
            || timestamp == message_ts
            || timestamp == thread_ts
        {
            return Err(TransportError::Response);
        }
        Ok(Some(OwnedProgressArtifact {
            channel: channel_id,
            timestamp: timestamp.to_owned(),
        }))
    }
    pub(super) async fn change_progress(
        &self,
        owned: &OwnedProgressArtifact,
        label: ActivityLabel,
    ) -> Result<(), TransportError> {
        if !self.progress_available.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut body = body(label);
        body["channel"] = json!(owned.channel);
        body["ts"] = json!(owned.timestamp);
        body["as_user"] = json!(true);
        let response = self.cosmetic_json("chat.update", &body).await?;
        if response["channel"] != owned.channel || response["ts"] != owned.timestamp {
            return Err(TransportError::Response);
        }
        Ok(())
    }
    pub(super) async fn remove_progress(
        &self,
        owned: &OwnedProgressArtifact,
    ) -> Result<(), TransportError> {
        // Even a disabled posting surface must attempt cleanup of this run's confirmed handle.
        match self
            .cosmetic_json(
                "chat.delete",
                &json!({"channel":owned.channel,"ts":owned.timestamp}),
            )
            .await
        {
            Ok(response)
                if response["channel"] == owned.channel && response["ts"] == owned.timestamp =>
            {
                Ok(())
            }
            Ok(_) => Err(TransportError::Response),
            Err(TransportError::Service { code }) if code == "message_not_found" => Ok(()),
            Err(error) => Err(error),
        }
    }
    pub(super) async fn cosmetic_json(
        &self,
        method: &str,
        body: &Value,
    ) -> Result<Value, TransportError> {
        // Reply sends do not acquire this mutex/budget; pending final sends win new cosmetic starts.
        if self.final_sends.load(Ordering::Acquire) != 0 {
            return Err(TransportError::ActivityRateLimited {
                retry_after: Duration::from_secs(2),
            });
        }
        if method == "chat.postMessage" {
            let channel = body["channel"].as_str().ok_or(TransportError::Response)?;
            self.post_rate
                .lock()
                .expect("Slack channel post rate")
                .reserve(channel, false)?;
        }
        // Last, so that a call the two local gates above refuse costs no installation budget: the
        // 30-per-minute window is what cleanup — chat.delete, reactions.remove — has to spend.
        self.cosmetic_rate
            .lock()
            .expect("Slack cosmetic rate")
            .reserve(Instant::now())?;
        let mut response = self
            .http
            .post(format!("{}/api/{method}", self.endpoint))
            .header(
                "authorization",
                format!("Bearer {}", self.bot_token.expose()),
            )
            .header("content-type", "application/json; charset=utf-8")
            .body(serde_json::to_vec(body).expect("JSON value serializes"))
            .timeout(ACTIVITY_REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?;
        if method == "chat.postMessage" {
            self.post_rate
                .lock()
                .expect("Slack channel post rate")
                .completed(
                    body["channel"].as_str().ok_or(TransportError::Response)?,
                    Duration::from_secs(1),
                );
        }
        let status = response.status();
        if status.as_u16() == 429 {
            let stated = stated_retry_after(&response);
            let retry_after = stated.unwrap_or(UNSTATED_COSMETIC_COOLDOWN);
            if method == "chat.postMessage" {
                // The ⌛ progress post is a `chat.postMessage` on the same physical channel as the
                // answer, so its 429 parks the slot every other sender in that channel waits on.
                // It goes through the same bound the final-post path uses: parking the shared slot
                // for the stated hour would drop every later answer there as `post-capacity`.
                self.post_rate
                    .lock()
                    .expect("Slack channel post rate")
                    .completed(
                        body["channel"].as_str().ok_or(TransportError::Response)?,
                        channel_post_park(stated),
                    );
            }
            self.cosmetic_rate
                .lock()
                .expect("Slack cosmetic rate")
                .cooldown = Some(Instant::now() + retry_after);
            return Err(TransportError::ActivityRateLimited { retry_after });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|source| TransportError::Request(Box::new(source)))?
        {
            if chunk.len() > 64 * 1024 - bytes.len() {
                return Err(TransportError::Response);
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: Value =
            serde_json::from_slice(&bytes).map_err(TransportError::MalformedResponse)?;
        if status.is_success() && body["ok"] == true {
            return Ok(body);
        }
        let code = if status.is_success() {
            // Closed vocabulary: raw platform text must not enter cosmetic diagnostics.
            match body["error"].as_str().unwrap_or("") {
                "missing_scope" => "missing_scope",
                "invalid_auth" => "invalid_auth",
                "token_revoked" => "token_revoked",
                "feature_disabled" => "feature_disabled",
                "not_allowed_token_type" => "not_allowed_token_type",
                "method_deprecated" => "method_deprecated",
                "deprecated_endpoint" => "deprecated_endpoint",
                "already_reacted" => "already_reacted",
                "no_reaction" => "no_reaction",
                "message_not_found" => "message_not_found",
                "cant_delete_message" => "cant_delete_message",
                "cant_update_message" => "cant_update_message",
                _ => "service-error",
            }
        } else {
            "http-error"
        };
        if matches!(
            code,
            "missing_scope"
                | "invalid_auth"
                | "token_revoked"
                | "not_allowed_token_type"
                | "cant_update_message"
                | "cant_delete_message"
        ) && method.starts_with("chat.")
        {
            self.progress_available.store(false, Ordering::Release);
        }
        Err(TransportError::Service { code: code.into() })
    }
}

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod rate_tests {
    use super::*;
    fn transport(name: &str) -> SlackTransport {
        SlackTransport::new(
            name.into(),
            "http://127.0.0.1:43219".into(),
            "fixture-app".into(),
            "fixture-bot".into(),
            SlackExperience::Agent,
            SlackActivityConfig::default(),
        )
        .unwrap()
    }
    #[test]
    fn authenticated_physical_installation_refuses_an_independent_configuration() {
        let first = transport("first");
        let second = transport("second");
        first.claim_installation("T1", "U1").unwrap();
        first.claim_installation("T1", "U1").unwrap();
        assert!(
            matches!(second.claim_installation("t1","u1"), Err(TransportError::Service {code}) if code=="duplicate-slack-installation")
        );
        drop(first);
        second.claim_installation("T1", "U1").unwrap();
    }
    #[test]
    fn terminal_retirement_bounds_failed_cleanup_metadata() {
        let transport = transport("retire");
        for i in 0..256 {
            let target = ActivityTarget::Slack {
                channel_id: "C1".into(),
                thread_ts: "1700000000.000001".into(),
                message_ts: format!("2.{i:06}"),
                initiator_user_id: "U1".into(),
            };
            transport
                .replier
                .update_attempt(&target, |attempt| attempt.agent_status = true);
            assert_eq!(transport.replier.active_activity.lock().unwrap().len(), 1);
            transport.replier.retire(&target);
        }
        assert!(transport.replier.active_activity.lock().unwrap().is_empty());
    }
    fn response(status: u16, body: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nRetry-After: 1\r\nConnection: close\r\n\r\n{body}",body.len()).into_bytes()
    }
    fn with_endpoint(endpoint: String, progress_message: bool) -> SlackTransport {
        SlackTransport::new(
            "fixture".into(),
            endpoint,
            "app".into(),
            "bot".into(),
            SlackExperience::Classic,
            SlackActivityConfig {
                mode: ActivityMode::Native,
                classic_fallback: SlackActivityFallback::Reaction,
                progress_message,
            },
        )
        .unwrap()
    }
    fn target() -> ActivityTarget {
        ActivityTarget::Slack {
            channel_id: "C1".into(),
            thread_ts: "1700000000.000001".into(),
            message_ts: "1700000000.000001".into(),
            initiator_user_id: "U1".into(),
        }
    }
    #[tokio::test(start_paused = true)]
    async fn a_channel_backoff_makes_later_posts_wait_rather_than_fail_instantly() {
        // No request is issued: this is the slot arithmetic a parked channel imposes on every
        // other sender, which is what a 429 used to turn into an instant `post-capacity`.
        let transport = with_endpoint("http://127.0.0.1:1".into(), false);
        let replier = &transport.replier;

        replier.channel_post_completed("C1", Duration::from_secs(30));
        let start = Instant::now();
        replier
            .reserve_channel_post("C1", Instant::now())
            .await
            .expect("a backoff inside the ceiling is waited out, not refused");
        assert!(
            start.elapsed() >= Duration::from_secs(30),
            "the honored backoff is waited through: {:?}",
            start.elapsed()
        );

        // A park that lands after this caller queued is dated from the 429, not from the entry, so
        // a full-ceiling park is still waited out by a sender that had already been waiting.
        replier.channel_post_completed("C2", POST_WAIT_CEILING);
        let start = Instant::now();
        replier
            .reserve_channel_post("C2", Instant::now() - Duration::from_secs(30))
            .await
            .expect("somebody else's full-ceiling park is waited out, not refused");
        assert!(
            start.elapsed() >= POST_WAIT_CEILING,
            "the whole park is waited through: {:?}",
            start.elapsed()
        );

        // Only a slot no park could have produced, or a caller already at its total budget, is
        // refused — and the refusal costs no further waiting.
        replier.channel_post_completed("C3", POST_WAIT_CEILING + Duration::from_secs(1));
        let start = Instant::now();
        assert!(
            matches!(
                replier.reserve_channel_post("C3", Instant::now()).await,
                Err(TransportError::Service { code }) if code == "post-capacity"
            ),
            "a slot beyond the per-observation ceiling is refused"
        );
        replier.channel_post_completed("C4", Duration::from_secs(30));
        assert!(
            matches!(
                replier
                    .reserve_channel_post("C4", Instant::now() - POST_TOTAL_WAIT_CEILING)
                    .await,
                Err(TransportError::Service { code }) if code == "post-capacity"
            ),
            "a caller that has spent its total budget is its own refusal"
        );
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "neither refusal waits first"
        );
    }

    #[tokio::test]
    async fn a_real_429_does_not_refuse_a_sender_that_entered_before_it() {
        // A real `Retry-After: 120` through the whole post path, then the sender that was already
        // in this channel when it landed — the population a Slack 429 implies. Its paid-for answer
        // waits the park out and posts; the park is dated from the 429, not from its own entry.
        let server = dekopon_test_support::LoopbackServer::sequence([
            b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            response(200, r#"{"ok":true,"channel":"C1","ts":"1700000000.000003"}"#),
        ]);
        let transport = with_endpoint(server.url(), false);
        let queued_entry = Instant::now();
        let reply_target = ReplyTarget::Slack {
            channel: "C1".into(),
            thread_ts: None,
        };
        let refused = transport
            .replier
            .reply(reply_target.clone(), OutboundReply::text("first"))
            .await
            .expect_err("a 120 s backoff is longer than this caller sits through");
        assert!(
            matches!(&refused, TransportError::Service { code } if code == "ratelimited"),
            "{refused:?}"
        );
        let park = transport.replier.post_rate.lock().unwrap().next["C1"];
        assert!(
            park > queued_entry + POST_WAIT_CEILING,
            "the park outlives a deadline frozen at the queued sender's entry"
        );

        // The waiting is virtual from here; the request above needed a real clock because a paused
        // one auto-advances through the client's own timeout while the socket is in flight.
        tokio::time::pause();
        transport
            .replier
            .reserve_channel_post("C1", queued_entry)
            .await
            .expect("the queued sender waits the park out rather than failing post-capacity");
        assert!(Instant::now() >= park, "it waited for the whole park");
        tokio::time::resume();
        assert!(
            transport
                .replier
                .reply(reply_target, OutboundReply::text("second"))
                .await
                .expect("the second session's answer posts")
                .accepted()
        );
        assert_eq!(server.recorded().len(), 2, "one post each, no retry");
    }

    #[tokio::test]
    async fn an_unbounded_retry_after_parks_the_channel_only_to_the_wait_ceiling() {
        for (header, parked) in [
            ("Retry-After: 120\r\n", POST_WAIT_CEILING),
            ("Retry-After: 86400\r\n", POST_WAIT_CEILING),
            ("", UNSTATED_RETRY_PENALTY),
            ("Retry-After: soon\r\n", UNSTATED_RETRY_PENALTY),
        ] {
            let server = dekopon_test_support::LoopbackServer::once(
                format!("HTTP/1.1 429 Too Many Requests\r\n{header}Content-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            );
            let transport = with_endpoint(server.url(), false);
            let error = transport
                .replier
                .reply(
                    ReplyTarget::Slack {
                        channel: "C1".into(),
                        thread_ts: None,
                    },
                    OutboundReply::text("refused"),
                )
                .await
                .expect_err("a backoff this caller cannot sit through is its own refusal");
            assert!(
                matches!(&error, TransportError::Service { code } if code == "ratelimited"),
                "{error:?}"
            );
            assert_eq!(
                server.recorded().len(),
                1,
                "no retry past the honored bound"
            );
            let slot = transport.replier.post_rate.lock().unwrap().next["C1"];
            let remaining = slot.saturating_duration_since(Instant::now());
            assert!(
                remaining <= parked && remaining + Duration::from_secs(2) >= parked,
                "{header:?} parks the slot for {parked:?}, not {remaining:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_429_on_the_progress_post_never_parks_the_channel_past_the_wait_ceiling() {
        // The ⌛ post is the other writer of the shared channel slot, and it draws the same
        // platform 429 the answer does. Parking the slot for the stated hour there dropped every
        // later answer in that channel — including the failure fallback — as `post-capacity`,
        // which is the finding's exact scenario reached through the sibling path.
        let server = dekopon_test_support::LoopbackServer::sequence([
            b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3600\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            response(200, r#"{"ok":true,"channel":"C1","ts":"1700000000.000003"}"#),
        ]);
        let transport = with_endpoint(server.url(), true);
        let replier = &transport.replier;
        let rate_limited = replier
            .cosmetic_json("chat.postMessage", &json!({"channel": "C1", "text": "⌛"}))
            .await
            .expect_err("the progress post is rate limited");
        assert!(
            matches!(&rate_limited, TransportError::ActivityRateLimited { retry_after }
                if *retry_after == Duration::from_secs(3600)),
            "the stating caller still learns the whole stated backoff: {rate_limited:?}"
        );
        let park = replier.post_rate.lock().unwrap().next["C1"];
        assert!(
            park <= Instant::now() + POST_WAIT_CEILING,
            "the shared slot is parked to the ceiling, not to the stated hour"
        );

        // Virtual from here: the park is a minute, and a real one would be a minute of test.
        tokio::time::pause();
        let start = Instant::now();
        replier
            .reserve_channel_post("C1", Instant::now())
            .await
            .expect("a following answer waits the progress post's park out, not `post-capacity`");
        assert!(
            start.elapsed() >= POST_WAIT_CEILING - Duration::from_secs(2),
            "it waited the park out: {:?}",
            start.elapsed()
        );
        tokio::time::resume();
        assert!(
            replier
                .reply(
                    ReplyTarget::Slack {
                        channel: "C1".into(),
                        thread_ts: None,
                    },
                    OutboundReply::text("the answer this session paid for"),
                )
                .await
                .expect("the answer posts once the park expires")
                .accepted()
        );
    }

    #[tokio::test]
    async fn locally_refused_cosmetics_leave_the_installation_budget_untouched() {
        let server = dekopon_test_support::LoopbackServer::sequence(Vec::<Vec<u8>>::new());
        let transport = with_endpoint(server.url(), true);
        // A pending final send sheds new cosmetics; the channel slot refuses them too.
        transport.replier.final_sends.fetch_add(1, Ordering::AcqRel);
        for _ in 0..10 {
            let error = transport
                .replier
                .cosmetic_json("chat.postMessage", &json!({"channel": "C1"}))
                .await
                .expect_err("a pending final send sheds a new cosmetic");
            assert!(matches!(error, TransportError::ActivityRateLimited { .. }));
        }
        transport.replier.final_sends.fetch_sub(1, Ordering::AcqRel);
        transport
            .replier
            .channel_post_completed("C1", Duration::from_secs(30));
        for _ in 0..10 {
            let error = transport
                .replier
                .cosmetic_json("chat.postMessage", &json!({"channel": "C1"}))
                .await
                .expect_err("a parked channel refuses a new cosmetic post");
            assert!(matches!(error, TransportError::ActivityRateLimited { .. }));
        }
        assert!(
            transport
                .replier
                .cosmetic_rate
                .lock()
                .unwrap()
                .requests
                .is_empty(),
            "twenty locally refused calls spent none of the 30-per-minute budget"
        );
        assert!(
            server.recorded().is_empty(),
            "nothing reached the service either"
        );
    }

    #[tokio::test]
    async fn progress_and_final_posts_share_channel_slots_and_recover_from_one_explicit_429() {
        for progress in [false, true] {
            let mut responses = Vec::new();
            if progress {
                responses.push(response(
                    200,
                    r#"{"ok":true,"channel":"C1","ts":"1700000000.000002"}"#,
                ));
            }
            responses.extend([
                response(429, "{}"),
                response(
                    200,
                    r#"{"ok":true,"channel":"C1","ts":"1700000000.000003"}"#,
                ),
                response(
                    200,
                    r#"{"ok":true,"channel":"C1","ts":"1700000000.000004"}"#,
                ),
            ]);
            let server = dekopon_test_support::LoopbackServer::sequence(responses);
            let transport = with_endpoint(server.url(), progress);
            if progress {
                assert!(
                    transport
                        .replier
                        .create_progress(target(), ActivityLabel::sanitized("Working"))
                        .await
                        .unwrap()
                        .is_some()
                );
            }
            let reply_target = ReplyTarget::Slack {
                channel: "C1".into(),
                thread_ts: Some("1700000000.000001".into()),
            };
            let (first, second) = tokio::join!(
                transport
                    .replier
                    .reply(reply_target.clone(), OutboundReply::text("same final")),
                transport
                    .replier
                    .reply(reply_target, OutboundReply::text("same final"))
            );
            assert!(first.unwrap().accepted());
            assert!(second.unwrap().accepted());
            let requests = server.recorded();
            assert_eq!(requests.len(), if progress { 4 } else { 3 });
            for request in requests.iter().skip(usize::from(progress)) {
                let request = String::from_utf8_lossy(request);
                assert!(request.contains("same final"));
            }
        }
    }
    #[tokio::test]
    async fn unknown_final_post_is_not_retried() {
        let server = dekopon_test_support::LoopbackServer::once(b"");
        let transport = with_endpoint(server.url(), false);
        assert!(
            transport
                .replier
                .reply(
                    ReplyTarget::Slack {
                        channel: "C1".into(),
                        thread_ts: None
                    },
                    OutboundReply::text("final")
                )
                .await
                .is_err()
        );
        assert_eq!(server.recorded().len(), 1);
    }
    #[tokio::test]
    async fn uncertain_native_write_survives_successful_reaction_fallback_and_cleanup() {
        let server = dekopon_test_support::LoopbackServer::sequence([
            vec![],
            response(200, r#"{"ok":true}"#),
            response(200, r#"{"ok":true}"#),
            response(200, r#"{"ok":true}"#),
        ]);
        let mut transport = with_endpoint(server.url(), false);
        Arc::get_mut(&mut transport.replier).unwrap().experience = SlackExperience::Agent;
        let error = transport.replier.show(target()).await.unwrap_err();
        assert!(crate::activity::uncertain(&error));
        transport.replier.hide(target()).await.unwrap();
        transport.replier.retire(&target());
        assert!(transport.replier.active_activity.lock().unwrap().is_empty());
        assert_eq!(server.recorded().len(), 4);
    }
    #[test]
    fn installation_budget_is_thirty_requests_per_rolling_minute() {
        let mut rate = CosmeticRate::default();
        let now = Instant::now();
        for _ in 0..30 {
            rate.reserve(now).unwrap();
        }
        assert!(
            matches!(rate.reserve(now),Err(TransportError::ActivityRateLimited { retry_after }) if retry_after == Duration::from_secs(60))
        );
        rate.reserve(now + Duration::from_secs(60)).unwrap();
    }
}
