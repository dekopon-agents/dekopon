pub(super) fn schema() -> serde_json::Value {
    let attributes = serde_json::Map::from_iter([
        ("delivery".to_owned(), serde_json::json!({"type": "String"})),
        ("event".to_owned(), serde_json::json!({"type": "String"})),
        (
            "action".to_owned(),
            serde_json::json!({"type": "String", "required": false}),
        ),
        (
            "at".to_owned(),
            serde_json::json!({"type": "Long", "required": false}),
        ),
        (
            "repository".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long"}, "fullName": {"type": "String"}}}),
        ),
        (
            "actor".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long"}, "kind": {"type": "String", "required": false}}}),
        ),
        (
            "number".to_owned(),
            serde_json::json!({"type": "Long", "required": false}),
        ),
        ("kind".to_owned(), serde_json::json!({"type": "String"})),
        (
            "pullRequest".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"baseRef": {"type": "String"}, "baseSha": {"type": "String"}, "merged": {"type": "Boolean"}, "crossRepository": {"type": "Boolean"}, "authorId": {"type": "Long"}, "authorAssociation": {"type": "String"}, "labels": {"type": "Set", "element": {"type": "String"}}, "mergedBy": {"type": "Long", "required": false}}, "required": false}),
        ),
        (
            "issue".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"authorId": {"type": "Long"}, "authorAssociation": {"type": "String"}, "labels": {"type": "Set", "element": {"type": "String"}}, "assignees": {"type": "Set", "element": {"type": "Long"}}, "milestone": {"type": "Long", "required": false}, "isPullRequest": {"type": "Boolean"}}, "required": false}),
        ),
        (
            "review".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long"}, "authorAssociation": {"type": "String"}}, "required": false}),
        ),
        (
            "comment".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long"}, "authorAssociation": {"type": "String"}}, "required": false}),
        ),
        (
            "push".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"ref": {"type": "String"}, "refType": {"type": "String"}, "before": {"type": "String"}, "after": {"type": "String"}, "created": {"type": "Boolean"}, "deleted": {"type": "Boolean"}, "forced": {"type": "Boolean"}, "commits": {"type": "Long"}, "baseRef": {"type": "String", "required": false}}, "required": false}),
        ),
        (
            "gitRef".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"name": {"type": "String", "required": false}, "refType": {"type": "String", "required": false}}, "required": false}),
        ),
        (
            "run".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long", "required": false}, "jobId": {"type": "Long", "required": false}, "runNumber": {"type": "Long", "required": false}, "attempt": {"type": "Long", "required": false}, "trigger": {"type": "String", "required": false}, "pullRequests": {"type": "Set", "element": {"type": "Long"}, "required": false}}, "required": false}),
        ),
        (
            "check".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long", "required": false}, "appId": {"type": "Long", "required": false}, "suiteId": {"type": "Long", "required": false}, "pullRequests": {"type": "Set", "element": {"type": "Long"}, "required": false}}, "required": false}),
        ),
        (
            "release".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long"}, "tag": {"type": "String"}, "draft": {"type": "Boolean"}, "prerelease": {"type": "Boolean"}, "targetCommitish": {"type": "String", "required": false}}, "required": false}),
        ),
        (
            "deployment".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long", "required": false}}, "required": false}),
        ),
        (
            "discussion".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"authorAssociation": {"type": "String"}}, "required": false}),
        ),
        (
            "alert".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"number": {"type": "Long", "required": false}, "state": {"type": "String", "required": false}, "resolution": {"type": "String", "required": false}, "severity": {"type": "String", "required": false}, "ecosystem": {"type": "String", "required": false}, "package": {"type": "String", "required": false}, "ghsa": {"type": "String", "required": false}}, "required": false}),
        ),
        (
            "label".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long"}, "name": {"type": "String"}}, "required": false}),
        ),
        (
            "milestone".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long"}, "number": {"type": "Long"}, "state": {"type": "String"}}, "required": false}),
        ),
        (
            "mergeGroup".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"headSha": {"type": "String"}, "baseSha": {"type": "String"}, "baseRef": {"type": "String"}, "headRef": {"type": "String", "required": false}}, "required": false}),
        ),
        (
            "package".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"id": {"type": "Long"}, "ecosystem": {"type": "String"}, "name": {"type": "String", "required": false}}, "required": false}),
        ),
        (
            "authorSet".to_owned(),
            serde_json::json!({"type": "Record", "attributes": {"actorLogin": {"type": "String"}, "headRef": {"type": "String", "required": false}, "headSha": {"type": "String", "required": false}, "headRepository": {"type": "String", "required": false}, "draft": {"type": "Boolean", "required": false}, "state": {"type": "String", "required": false}, "reviewState": {"type": "String", "required": false}, "reviewCommit": {"type": "String", "required": false}, "commentCommit": {"type": "String", "required": false}, "runStatus": {"type": "String", "required": false}, "conclusion": {"type": "String", "required": false}, "runPath": {"type": "String", "required": false}, "alertRef": {"type": "String", "required": false}, "forkRepository": {"type": "String", "required": false}, "category": {"type": "String", "required": false}, "answered": {"type": "Boolean", "required": false}, "environment": {"type": "String", "required": false}, "alertState": {"type": "String", "required": false}, "deploymentRef": {"type": "String", "required": false}, "deploymentSha": {"type": "String", "required": false}, "deploymentState": {"type": "String", "required": false}, "task": {"type": "String", "required": false}}, "required": false}),
        ),
        (
            "member".to_owned(),
            serde_json::json!({"type": "Long", "required": false}),
        ),
    ]);
    serde_json::json!({"type":"Record","required":false,"attributes":attributes})
}
