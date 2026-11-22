use super::*;
use dekopon_agent::BrokerLeg;
use dekopon_agent::grant::{
    GrantLedger, GrantLedgerFailed, GrantRefusal, GrantRelease, SessionGrants,
};
use dekopon_core::VmSession;
use dekopon_model_token_governor::{
    Budget, Call, Estimate, MeterSpec, Metering, Tokens, UnixMillis, Via,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const AGENT: &str = "astra";
const MODEL: &str = "claude-ultra";
const SESSION_A: &str = "01937b2e-7d1a-7c3e-8f00-000000000001";
const SESSION_B: &str = "01937b2e-7d1a-7c3e-8f00-000000000002";

fn session(text: &str) -> VmSession {
    VmSession::parse(text).expect("fixture session is a canonical UUIDv7")
}

fn metering(limit: u64) -> Arc<Metering> {
    Arc::new(Metering::new(
        vec![Budget::new(
            AGENT.parse().unwrap(),
            None,
            &[MeterSpec::Fixed {
                limit: Tokens(limit),
                period: Duration::from_secs(24 * 3_600),
            }],
            UnixMillis(0),
        )],
        Arc::new(|| UnixMillis(3_600_000)),
    ))
}

fn used(metering: &Metering) -> u64 {
    metering.statuses(&AGENT.parse().unwrap()).unwrap()[0]
        .1
        .used
        .0
}

fn chat_fits(metering: &Arc<Metering>, tokens: u64) -> bool {
    metering
        .admit(
            Call {
                agent: AGENT.parse().unwrap(),
                model: MODEL.to_owned(),
                backend: "codex",
                via: Via::Agent,
            },
            Estimate {
                input: Tokens(tokens),
                output_reserve: Tokens(0),
            },
        )
        .map(|admission| admission.settle(dekopon_model_token_governor::Outcome::NotSent))
        .is_ok()
}

#[derive(Default)]
struct FakeVm {
    runs: AtomicU32,
}

impl FakeVm {
    fn exec(&self, leg: &BrokerLeg, session: VmSession, estimate: u64) -> Result<(), GrantRefusal> {
        if let Some(grants) = leg.grants() {
            grants.carve(session, MODEL, Tokens(estimate))?;
        }
        self.runs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn poll(&self) {
        self.runs.fetch_add(1, Ordering::Relaxed);
    }

    fn runs(&self) -> u32 {
        self.runs.load(Ordering::Relaxed)
    }
}

async fn leg_with(grants: &Arc<SessionGrants>, directory: &Path) -> BrokerLeg {
    let (broker, _observed) = stub_broker(directory, listings(1, &[])).await;
    BrokerLeg::connect(broker.leg_client().unwrap(), None)
        .await
        .unwrap()
        .with_grants(Arc::clone(grants))
}

#[tokio::test]
async fn a_grant_whose_tally_never_returns_is_charged_in_full() {
    let directory = temporary();
    let metering = metering(100);
    let grants = Arc::new(SessionGrants::new(
        Arc::clone(&metering),
        AGENT.parse().unwrap(),
    ));
    let leg = leg_with(&grants, directory.path()).await;
    let vm = FakeVm::default();
    vm.exec(&leg, session(SESSION_A), 60).unwrap();
    assert!(!chat_fits(&metering, 50), "the open grant holds 60 of 100");
    leg.grants().unwrap().release(&session(SESSION_A));
    assert_eq!(used(&metering), 60);
    assert!(!chat_fits(&metering, 50), "the whole grant was charged");
    assert!(chat_fits(&metering, 40));
}

#[tokio::test]
async fn a_returned_tally_settles_the_hold_and_refunds_the_rest() {
    let directory = temporary();
    let metering = metering(100);
    let grants = Arc::new(SessionGrants::new(
        Arc::clone(&metering),
        AGENT.parse().unwrap(),
    ));
    let leg = leg_with(&grants, directory.path()).await;
    let vm = FakeVm::default();
    vm.exec(&leg, session(SESSION_A), 60).unwrap();
    assert!(!chat_fits(&metering, 50));
    leg.grants().unwrap().tally(&session(SESSION_A), Tokens(10));
    assert_eq!(used(&metering), 10);
    assert!(
        chat_fits(&metering, 90),
        "the unspent 50 returned to the cap"
    );
    leg.grants().unwrap().tally(&session(SESSION_A), Tokens(10));
    leg.grants().unwrap().release_all();
    assert_eq!(
        used(&metering),
        10,
        "a late tally and the turn's end charge nothing more"
    );
}

#[tokio::test]
async fn a_grant_is_kept_across_calls_and_released_with_its_turn() {
    let directory = temporary();
    let metering = metering(1_000);
    let grants = Arc::new(SessionGrants::new(
        Arc::clone(&metering),
        AGENT.parse().unwrap(),
    ));
    let leg = leg_with(&grants, directory.path()).await;
    let vm = FakeVm::default();
    for index in 0..4 {
        let text = format!("01937b2e-7d1a-7c3e-8f00-00000000010{index}");
        vm.exec(&leg, session(&text), 100).unwrap();
    }
    assert!(matches!(
        vm.exec(&leg, session(SESSION_B), 100),
        Err(GrantRefusal::Full)
    ));
    let first = session("01937b2e-7d1a-7c3e-8f00-000000000100");
    vm.poll();
    vm.exec(&leg, first, 300).unwrap();
    assert!(
        chat_fits(&metering, 600),
        "a repeated carve reuses the 100 held"
    );
    assert!(!chat_fits(&metering, 601));
    leg.grants().unwrap().tally(&first, Tokens(10));
    assert_eq!(
        used(&metering),
        10,
        "one tally settles the session's one grant"
    );
    grants.release_all();
    assert_eq!(used(&metering), 310);
    assert!(chat_fits(&metering, 690));
    assert_eq!(vm.runs(), 6);
}

#[tokio::test]
async fn a_carve_the_budget_cannot_cover_is_refused_before_the_vm_runs() {
    let directory = temporary();
    let metering = metering(100);
    let grants = Arc::new(SessionGrants::new(
        Arc::clone(&metering),
        AGENT.parse().unwrap(),
    ));
    let leg = leg_with(&grants, directory.path()).await;
    let vm = FakeVm::default();
    vm.exec(&leg, session(SESSION_A), 80).unwrap();
    vm.exec(&leg, session(SESSION_B), 80).unwrap();
    assert!(
        !chat_fits(&metering, 1),
        "the second carve was clamped to the last 20"
    );
    let refused = vm.exec(&leg, session("01937b2e-7d1a-7c3e-8f00-000000000003"), 1);
    assert!(matches!(refused, Err(GrantRefusal::Budget(refusal)) if refusal.remaining == 0));
    leg.grants().unwrap().release_all();
    let zero = vm.exec(&leg, session(SESSION_A), 0);
    assert!(matches!(zero, Err(GrantRefusal::Budget(_))));
    assert_eq!(vm.runs(), 2);
}

struct PausedLedger {
    entered: std::sync::mpsc::SyncSender<()>,
    verdicts: Mutex<std::sync::mpsc::Receiver<Result<(), GrantLedgerFailed>>>,
}

impl GrantLedger for PausedLedger {
    fn carved(
        &self,
        _: &dekopon_core::AgentId,
        _: &VmSession,
        _: &str,
        _: Tokens,
    ) -> Result<(), GrantLedgerFailed> {
        self.entered.send(()).unwrap();
        self.verdicts
            .lock()
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or(Err(GrantLedgerFailed))
    }

    fn tallied(&self, _: &VmSession, _: Tokens) -> Result<(), GrantLedgerFailed> {
        Ok(())
    }

    fn released(&self, _: &VmSession, _: GrantRelease) -> Result<(), GrantLedgerFailed> {
        Ok(())
    }
}

#[tokio::test]
async fn a_same_session_carve_waits_for_the_ledger_and_cannot_reuse_a_refused_hold() {
    let directory = temporary();
    let metering = metering(100);
    let (entered, carving) = std::sync::mpsc::sync_channel(0);
    let (verdict, verdicts) = std::sync::mpsc::sync_channel(0);
    let grants = Arc::new(
        SessionGrants::new(Arc::clone(&metering), AGENT.parse().unwrap()).with_ledger(Arc::new(
            PausedLedger {
                entered,
                verdicts: Mutex::new(verdicts),
            },
        )),
    );
    let leg = leg_with(&grants, directory.path()).await;
    let vm = FakeVm::default();
    let second_done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| vm.exec(&leg, session(SESSION_A), 60));
        carving.recv_timeout(Duration::from_secs(10)).unwrap();
        let second = scope.spawn(|| {
            let carved = vm.exec(&leg, session(SESSION_A), 60);
            second_done.store(true, Ordering::SeqCst);
            carved
        });
        for _ in 0..1_000 {
            std::thread::yield_now();
        }
        assert!(
            !second_done.load(Ordering::SeqCst),
            "the second carve returned before the first one's ledger write"
        );
        verdict.send(Err(GrantLedgerFailed)).unwrap();
        assert!(matches!(
            first.join().unwrap(),
            Err(GrantRefusal::Ledger(GrantLedgerFailed))
        ));
        carving.recv_timeout(Duration::from_secs(10)).unwrap();
        verdict.send(Ok(())).unwrap();
        second.join().unwrap().unwrap();
    });
    assert_eq!(vm.runs(), 1);
    grants.release_all();
    assert_eq!(
        used(&metering),
        60,
        "the surviving grant is charged in full"
    );
}
