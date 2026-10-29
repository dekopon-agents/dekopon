use super::*;
use crate::{pull_request_links::PullRequestLinkContext, wake::Anchor};

fn subscription_anchor() -> Anchor {
    Anchor::from_inbound(
        &owned_slack_message("review", false),
        &route(model_config()).agent,
    )
    .unwrap()
    .as_space()
    .unwrap()
}

#[test]
fn a_dotted_repository_is_refused_at_pr_link_parse_and_persists_nothing() {
    let directory = temporary();
    let store = wakes::wake_store(directory.path());
    let before = dekopon_test_support::snapshot_tree(directory.path());
    let refusal =
        PullRequestLinkContext::parse("https://github.com/other/repo.name/pull/18").unwrap_err();
    assert_eq!(
        refusal,
        ExternalSubject::github("other", "repo.name").unwrap_err()
    );
    assert!(matches!(
        refusal,
        dekopon_core::SubjectError::InvalidSegment {
            segment: "subject",
            ..
        }
    ));
    assert_eq!(
        dekopon_test_support::snapshot_tree(directory.path()),
        before
    );
    assert!(
        store
            .list_subscriptions(&subscription_anchor(), std::time::SystemTime::now())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_sixty_fifth_subscription_is_refused_with_a_typed_error() {
    let directory = temporary();
    let store = wakes::wake_store(directory.path());
    let anchor = subscription_anchor();
    let now = std::time::SystemTime::now();
    for number in 1..=64 {
        let target =
            PullRequestLinkContext::parse(&format!("https://github.com/acme/widget/pull/{number}"))
                .unwrap()
                .unwrap()
                .targets
                .remove(0);
        store.subscribe(anchor.clone(), target, now).unwrap();
    }
    let target = PullRequestLinkContext::parse("https://github.com/acme/widget/pull/65")
        .unwrap()
        .unwrap()
        .targets
        .remove(0);
    assert_eq!(
        store.subscribe(anchor.clone(), target, now),
        Err(dekopon_agent::wake::WakeRefusal::SubscriptionsFull { maximum: 64 })
    );
    assert_eq!(store.list_subscriptions(&anchor, now).unwrap().len(), 64);
}

#[test]
fn a_subscription_expires_at_its_fourteen_day_deadline_without_firing() {
    let directory = temporary();
    let store = wakes::wake_store(directory.path());
    let anchor = subscription_anchor();
    let created = std::time::SystemTime::now();
    let target = PullRequestLinkContext::parse("https://github.com/acme/widget/pull/17")
        .unwrap()
        .unwrap()
        .targets
        .remove(0);
    let summary = store
        .subscribe(anchor.clone(), target.clone(), created)
        .unwrap();
    let expiry = Duration::from_secs(14 * 24 * 60 * 60);
    assert_eq!(summary.due_in, expiry);
    let repeated = store
        .subscribe(anchor.clone(), target, created + Duration::from_secs(60))
        .unwrap();
    assert_eq!(repeated.id, summary.id);
    assert_eq!(repeated.due_in, expiry - Duration::from_secs(60));
    let due = store.take_due(created + expiry).unwrap();
    assert!(due.fired.is_empty());
    assert!(due.ticks.is_empty());
    assert!(
        store
            .list_subscriptions(&anchor, created + expiry)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn only_a_confirmed_review_of_a_link_target_creates_a_space_subscription() {
    use dekopon_shell::CapabilityCallResult;
    let directory = temporary();
    let store = wakes::wake_store(directory.path());
    let observer = crate::wake::ReviewSubscriptions {
        anchor: subscription_anchor(),
        store: Arc::clone(&store),
        links: PullRequestLinkContext::parse("https://github.com/acme/widget/pull/17")
            .unwrap()
            .unwrap(),
    };
    let input = json!({"owner":"acme","repo":"widget","number":17});
    observer.reviewed(
        "gh.pull-request.comment",
        &input,
        CapabilityCallResult::Denied {
            reason: "policy".into(),
        },
    );
    observer.reviewed(
        "gh.pull-request.comment",
        &json!({"owner":"acme","repo":"other","number":17}),
        CapabilityCallResult::Succeeded,
    );
    assert!(
        store
            .list_subscriptions(&subscription_anchor(), std::time::SystemTime::now())
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        observer.reviewed(
            "gh.pull-request.comment",
            &input,
            CapabilityCallResult::Succeeded
        ),
        CapabilityCallResult::Succeeded
    ));
    let rows = fs::read_to_string(directory.path().join("wakes/wakes.jsonl")).unwrap();
    let row: Value = serde_json::from_str(rows.trim()).unwrap();
    assert_eq!(row["subject"], subscription_anchor().subject().canonical());
    assert_eq!(
        row["subscription"],
        json!({"repository":"acme/widget","number":17})
    );
    assert_eq!(
        store
            .list_subscriptions(&subscription_anchor(), std::time::SystemTime::now())
            .unwrap()
            .len(),
        1
    );
}
