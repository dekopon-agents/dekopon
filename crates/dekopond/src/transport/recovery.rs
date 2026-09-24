use std::{sync::Arc, time::Duration};

use futures_util::future::BoxFuture;
use tokio::time::{Instant, sleep, timeout};

use super::{
    AssetFetcher, ChatDriver, ChatTransport, ThreadOwnership, TransportError, TransportEvent,
    TransportIdentity, reconnect_delay,
};

const MAX_FAILURES: u32 = 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTHY_RESET: Duration = Duration::from_secs(5 * 60);

pub(crate) struct RecoveringTransport {
    inner: Box<dyn ChatTransport>,
    failures: u32,
    connected_since: Option<Instant>,
    identity: Option<TransportIdentity>,
}

impl RecoveringTransport {
    pub(crate) fn new(inner: Box<dyn ChatTransport>) -> Self {
        Self {
            inner,
            failures: 0,
            connected_since: None,
            identity: None,
        }
    }

    async fn failed(&mut self, error: TransportError) -> Result<(), TransportError> {
        if self
            .connected_since
            .take()
            .is_some_and(|since| since.elapsed() >= HEALTHY_RESET)
        {
            self.failures = 0;
        }
        self.failures = self.failures.saturating_add(1);
        if !self.inner.retryable(&error) {
            return Err(error);
        }
        if self.failures >= MAX_FAILURES {
            return Err(TransportError::RecoveryExhausted {
                failures: self.failures,
                source: Box::new(error),
            });
        }
        let delay = reconnect_delay(self.failures - 1);
        tracing::warn!(
            event = "gateway_transport_recovering",
            transport = self.name(),
            category = error.category(),
            failure = self.failures,
            delay_ms = delay.as_millis() as u64,
        );
        sleep(delay).await;
        Ok(())
    }

    async fn open(&mut self) -> Result<TransportIdentity, TransportError> {
        loop {
            let attempt = if self.identity.is_some() {
                self.inner.reconnect()
            } else {
                self.inner.connect()
            };
            let result = match timeout(CONNECT_TIMEOUT, attempt).await {
                Ok(result) => result,
                Err(_) => Err(TransportError::ConnectTimeout),
            };
            match result {
                Ok(identity) => {
                    if self
                        .identity
                        .as_ref()
                        .is_some_and(|previous| previous != &identity)
                    {
                        return Err(TransportError::IdentityChanged);
                    }
                    self.identity = Some(identity.clone());
                    self.connected_since = Some(Instant::now());
                    tracing::info!(
                        event = "gateway_transport_connected",
                        transport = self.name()
                    );
                    return Ok(identity);
                }
                Err(error) => self.failed(error).await?,
            }
        }
    }
}

impl ChatTransport for RecoveringTransport {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
        Box::pin(self.open())
    }

    fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
        Box::pin(async move {
            loop {
                match self.inner.next().await {
                    Ok(event) => return Ok(event),
                    Err(error) => {
                        self.failed(error).await?;
                        self.open().await?;
                    }
                }
            }
        })
    }

    fn driver(&self) -> Arc<dyn ChatDriver> {
        self.inner.driver()
    }
    fn asset_fetcher(&self) -> Option<Arc<dyn AssetFetcher>> {
        self.inner.asset_fetcher()
    }
    fn thread_ownership(&self) -> Option<Arc<dyn ThreadOwnership>> {
        self.inner.thread_ownership()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::transport::{OutboundReply, ReplyTarget};

    struct NoWrites;
    #[async_trait]
    impl ChatDriver for NoWrites {
        async fn reply(&self, _: &ReplyTarget, _: OutboundReply) -> Result<(), TransportError> {
            panic!("connection recovery must never send a message")
        }
    }

    struct Scripted {
        opens: VecDeque<(Duration, bool)>,
        reads: VecDeque<Duration>,
        attempts: Arc<Mutex<Vec<Instant>>>,
        driver: Arc<dyn ChatDriver>,
    }

    impl ChatTransport for Scripted {
        fn name(&self) -> &str {
            "scripted"
        }
        fn connect(&mut self) -> BoxFuture<'_, Result<TransportIdentity, TransportError>> {
            Box::pin(async move {
                self.attempts.lock().expect("attempts").push(Instant::now());
                let Some((delay, success)) = self.opens.pop_front() else {
                    return std::future::pending().await;
                };
                sleep(delay).await;
                if success {
                    Ok(TransportIdentity::default())
                } else {
                    Err(TransportError::Closed)
                }
            })
        }
        fn next(&mut self) -> BoxFuture<'_, Result<TransportEvent, TransportError>> {
            Box::pin(async move {
                let Some(delay) = self.reads.pop_front() else {
                    return std::future::pending().await;
                };
                sleep(delay).await;
                Err(TransportError::Closed)
            })
        }
        fn driver(&self) -> Arc<dyn ChatDriver> {
            Arc::clone(&self.driver)
        }
    }

    fn scripted(
        opens: impl IntoIterator<Item = (Duration, bool)>,
        reads: Vec<Duration>,
    ) -> RecoveringTransport {
        RecoveringTransport::new(Box::new(Scripted {
            opens: opens.into_iter().collect(),
            reads: reads.into(),
            attempts: Arc::default(),
            driver: Arc::new(NoWrites),
        }))
    }

    #[tokio::test(start_paused = true)]
    async fn initial_failure_counts_and_exponential_backoff_caps_before_exhaustion() {
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let mut transport = RecoveringTransport::new(Box::new(Scripted {
            opens: vec![(Duration::ZERO, false); 10].into(),
            reads: VecDeque::new(),
            attempts: Arc::clone(&attempts),
            driver: Arc::new(NoWrites),
        }));
        let error = transport.connect().await.expect_err("ten failures exhaust");
        assert!(
            matches!(error, TransportError::RecoveryExhausted { failures: 10, ref source } if matches!(**source, TransportError::Closed))
        );
        let attempts = attempts.lock().expect("attempts");
        assert_eq!(attempts.len(), 10);
        for (index, pair) in attempts.windows(2).enumerate() {
            let floor = Duration::from_millis(500)
                .saturating_mul(1 << index)
                .min(Duration::from_secs(60));
            let delay = pair[1] - pair[0];
            assert!(
                delay >= floor && delay < floor + Duration::from_millis(252),
                "{index}: {delay:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_connection_attempt_has_a_deadline_and_a_finite_budget() {
        let mut transport = scripted([], vec![]);
        let start = Instant::now();
        let error = transport
            .connect()
            .await
            .expect_err("hung attempts exhaust too");
        assert!(
            matches!(error, TransportError::RecoveryExhausted { failures: 10, ref source } if matches!(**source, TransportError::ConnectTimeout))
        );
        assert!(start.elapsed() >= CONNECT_TIMEOUT * 10);
        assert!(start.elapsed() < Duration::from_secs(550));
    }

    #[tokio::test(start_paused = true)]
    async fn startup_recovery_succeeds_without_resetting_the_episode() {
        let mut transport = scripted([(Duration::ZERO, false), (Duration::ZERO, true)], vec![]);
        transport.connect().await.expect("second attempt connects");
        assert_eq!(transport.failures, 1);
        assert!(transport.connected_since.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn flapping_connections_exhaust_even_when_every_reconnect_succeeds() {
        let mut transport = scripted(
            vec![(Duration::ZERO, true); 10],
            vec![Duration::from_secs(1); 10],
        );
        transport.connect().await.expect("initial connection");
        let error = transport
            .next()
            .await
            .expect_err("flapping must not reset the budget");
        assert!(matches!(
            error,
            TransportError::RecoveryExhausted { failures: 10, .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn five_continuously_connected_idle_minutes_reset_the_budget() {
        let mut transport = scripted([(Duration::ZERO, false), (Duration::ZERO, true)], vec![]);
        transport.connect().await.expect("recovered");
        tokio::time::advance(HEALTHY_RESET).await;
        transport
            .failed(TransportError::Closed)
            .await
            .expect("new episode");
        assert_eq!(
            transport.failures, 1,
            "idle health does not require user traffic"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_backoff_or_a_hung_connect_drops_the_attempt_immediately() {
        for hung in [false, true] {
            let mut transport = scripted(
                if hung {
                    vec![]
                } else {
                    vec![(Duration::ZERO, false)]
                },
                vec![],
            );
            let start = Instant::now();
            let task = tokio::spawn(async move { transport.connect().await });
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(100)).await;
            task.abort();
            assert!(task.await.expect_err("aborted").is_cancelled());
            assert!(start.elapsed() < Duration::from_millis(500));
        }
    }

    #[test]
    fn composing_recovery_preserves_the_original_reply_driver() {
        let transport = scripted([], vec![]);
        assert!(Arc::ptr_eq(&transport.driver(), &transport.inner.driver()));
    }
    #[tokio::test(start_paused = true)]
    async fn exhaustion_wakes_supervision_while_a_healthy_reader_is_still_alive() {
        let (sender, mut events) = tokio::sync::mpsc::channel(4);
        let mut readers = tokio::task::JoinSet::new();
        readers.spawn(crate::read_transport(
            Box::new(scripted([(Duration::ZERO, true)], vec![])),
            sender.clone(),
        ));
        readers.spawn(crate::read_transport(
            Box::new(scripted(vec![(Duration::ZERO, false); 10], vec![])),
            sender,
        ));
        assert!(
            matches!(events.recv().await, Some(TransportEvent::Connected { .. })),
            "healthy reader becomes available while its peer retries"
        );
        let error = crate::supervise_transports(&mut readers, std::future::pending())
            .await
            .expect_err("one exhausted reader is enough to stop the gateway");
        let crate::DekopondError::TransportConnect { problems } = error else {
            panic!("expected transport failure");
        };
        assert!(matches!(
            problems[0].source,
            TransportError::RecoveryExhausted { failures: 10, .. }
        ));
        assert_eq!(
            readers.len(),
            1,
            "healthy reader was not the reason supervision stopped"
        );
        readers.abort_all();
        while let Some(result) = readers.join_next().await {
            assert!(result.expect_err("aborted healthy reader").is_cancelled());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn supervision_observes_reader_panics_instead_of_leaving_a_green_gateway() {
        let mut readers = tokio::task::JoinSet::new();
        readers.spawn(async { panic!("synthetic reader panic") });
        let error = crate::supervise_transports(&mut readers, std::future::pending())
            .await
            .expect_err("task failure stops gateway");
        assert!(matches!(error, crate::DekopondError::TransportTask(source) if source.is_panic()));
    }
    #[tokio::test(start_paused = true)]
    async fn a_changed_identity_is_terminal_instead_of_using_stale_routing_metadata() {
        let mut transport = scripted([(Duration::ZERO, true)], vec![]);
        transport.identity = Some(TransportIdentity {
            user_id: Some("previous-bot".to_owned()),
            handle: None,
        });
        let error = transport
            .open()
            .await
            .expect_err("adapter returned a different identity");
        assert!(matches!(error, TransportError::IdentityChanged));
        assert_eq!(transport.failures, 0, "identity change is never retried");
    }
}
