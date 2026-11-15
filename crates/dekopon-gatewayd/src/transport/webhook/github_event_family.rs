use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::{
    gateway::CurrentGeneration,
    transport::{
        TransportEvent,
        github::{
            Acked, GithubTransport,
            tests::{pr, signed},
        },
    },
};

async fn github(events: Value) -> GithubTransport {
    let directory = crate::tests::temporary();
    std::fs::write(
        directory.path().join("dekopon.yaml"),
        crate::tests::catalog_text(true, Some("reasoning")),
    )
    .expect("catalog");
    let mut doc = crate::tests::document(directory.path());
    doc["transports"][0] = json!({"kind":"githubWebhook", "name":"github",
        "bind":"127.0.0.1:9080", "secretEnv":"PATH"});
    doc["routes"][0]["transport"] = json!("github");
    doc["routes"][0]["memory"] = json!({"mode":"oneShot"});
    doc["routes"][0]["conversation"] = json!({"kind":["thread"],
        "container":"owner", "ids":["owner.repo"]});
    doc["routes"][0]["githubEvents"] = events;
    let resolved = crate::tests::load(directory.path(), &doc)
        .await
        .expect("config");
    let catalog =
        dekopon_config::LocalCatalog::load(directory.path().join("dekopon.yaml")).expect("catalog");
    let routes = crate::routes::RoutingTable::bind(&resolved, &catalog).expect("route");
    GithubTransport::new(
        "github".to_owned(),
        "127.0.0.1:9080".parse().expect("bind"),
        "secret".to_owned(),
        CurrentGeneration::boot(Arc::new(routes), Arc::default()),
        None,
    )
}

#[tokio::test]
async fn github_event_family_cannot_be_chosen_by_the_unsigned_header() {
    let repository = json!({"id": 10, "owner": {"login": "Owner"}, "name": "Repo"});
    let sender = json!({"id": 99, "login": "outsider", "type": "User"});
    let issue_comment = json!({
        "action": "created", "repository": repository, "sender": sender,
        "issue": {"number": 7, "user": {"id": 1}, "author_association": "OWNER",
            "labels": [], "assignees": [], "state": "open"},
        "comment": {"id": 2, "author_association": "NONE"}
    });
    for (allowed, actual, body) in [
        ("issues", "issue_comment", issue_comment.clone()),
        ("repository_dispatch", "issue_comment", issue_comment),
        (
            "discussion",
            "discussion_comment",
            json!({
                "action": "created", "repository": repository, "sender": sender,
                "discussion": {"number": 7, "author_association": "OWNER", "state": "open"},
                "comment": {"id": 2, "author_association": "NONE"}
            }),
        ),
        (
            "deployment",
            "deployment_status",
            json!({
                "action": "created", "repository": repository, "sender": sender,
                "deployment": {"id": 7, "ref": "main", "sha": "a".repeat(40)},
                "deployment_status": {"state": "success"}
            }),
        ),
    ] {
        let mut transport = github(json!([allowed])).await;
        let body = serde_json::to_vec(&body).expect("body");
        let honest = transport
            .request_for_test(signed(
                &body,
                b"secret",
                "33333333-3333-3333-3333-333333333333",
                actual,
            ))
            .await;
        assert_eq!(honest.status(), StatusCode::OK);
        assert!(transport.try_delivery_for_test().is_err());

        let relabelled = transport.request_for_test(signed(
            &body,
            b"secret",
            "44444444-4444-4444-4444-444444444444",
            allowed,
        ));
        tokio::pin!(relabelled);
        let admitted = tokio::select! {
            biased;
            _ = &mut relabelled => false,
            _ = transport.delivery_for_test() => true,
        };
        assert!(!admitted, "{actual} body was admitted as {allowed}");
    }
}

#[tokio::test]
async fn unclassified_push_body_is_ignored_under_a_listed_header() {
    let mut transport = github(json!(["repository_dispatch"])).await;
    let body = serde_json::to_vec(&json!({
        "repository": {"id": 10, "owner": {"login": "Owner"}, "name": "Repo"},
        "sender": {"id": 99, "login": "outsider", "type": "User"},
        "ref": "refs/heads/main",
        "before": "a".repeat(40),
        "after": "b".repeat(40),
        "created": false,
        "deleted": false,
        "forced": false,
        "commits": []
    }))
    .expect("body");
    let relabelled = transport.request_for_test(signed(
        &body,
        b"secret",
        "55555555-5555-5555-5555-555555555555",
        "repository_dispatch",
    ));
    tokio::pin!(relabelled);
    let admitted = tokio::select! {
        biased;
        _ = &mut relabelled => false,
        _ = transport.delivery_for_test() => true,
    };
    assert!(!admitted, "a push body reached a repository_dispatch route");
}

#[tokio::test]
async fn a_null_family_key_does_not_obscure_a_signed_pull_request() {
    let mut transport = github(json!(["pull_request"])).await;
    let mut body = pr();
    body["issue"] = Value::Null;
    let body = serde_json::to_vec(&body).expect("body");
    let request = transport.request_for_test(signed(
        &body,
        b"secret",
        "66666666-6666-6666-6666-666666666666",
        "pull_request",
    ));
    tokio::pin!(request);
    let event = tokio::select! {
        biased;
        _ = &mut request => panic!("a valid pull request waits for admission"),
        event = transport.delivery_for_test() => event,
    };
    let TransportEvent::Delivery(_, ticket) = event else {
        panic!("delivery ticket")
    };
    ticket.answer(Acked::Admitted);
    assert_eq!(request.await.status(), StatusCode::OK);
}
