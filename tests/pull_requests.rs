use axum::{
    extract::{Path, Query},
    routing::{get, post},
    Json, Router,
};
use factories::pull_requests::*;
use serde_json::{json, Value};
use std::collections::HashMap;

#[test]
fn links_derive_repository_and_provider_without_manual_metadata() {
    assert_eq!(
        parse_link("https://github.com/team/repo/pull/12").unwrap(),
        (
            "https://github.com/team/repo".into(),
            "github_pr".into(),
            "12".into()
        )
    );
    assert_eq!(
        parse_link("https://dev.azure.com/org/project/_git/repo/pullrequest/123").unwrap(),
        (
            "https://dev.azure.com/org/project/_git/repo".into(),
            "ado_pr".into(),
            "123".into()
        )
    );
    for bad in [
        "http://github.com/o/r/pull/1",
        "https://token@github.com/o/r/pull/1",
        "https://github.com/o/r/pull/0",
        "https://github.com/o/r/pull/1?redirect=evil",
        "https://github.com/o/r/issues/1",
    ] {
        assert!(parse_link(bad).is_err(), "{bad}");
    }
    let issue = metadata_issue(
        "github",
        "12",
        "https://github.com/o/r/pull/12",
        &json!({"title":"Actual PR title","body":"Actual description"}),
        Some(json!({"title":"Acceptance"})),
    )
    .unwrap();
    assert_eq!(issue.title, "Actual PR title");
    assert_eq!(issue.body, "Actual description");
    assert!(issue.ticket.is_some());
    assert!(metadata_issue("github", "12", "url", &json!({}), None).is_err());
}

#[test]
fn inline_locations_account_for_deletions_and_multiple_hunks() {
    let diff = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -5,3 +5,3 @@\n context\n-old\n+new\n tail\n@@ -50 +50,2 @@\n same\n+added\n";
    for line in [5, 6, 7, 50, 51] {
        assert!(right_line_in_diff(diff, line));
    }
    for line in [1, 4, 8, 49, 52] {
        assert!(!right_line_in_diff(diff, line));
    }
    assert!(!right_line_in_diff("@@ -1 +0,0 @@\n-removed", 1));
}

#[test]
fn finding_contract_rejects_invalid_or_unbounded_reports() {
    let valid = json!({"findings":[{"file":"src/file.rs","line":10,"severity":"medium","message":"Breaks on empty input","existing_comment_id":42}]});
    assert_eq!(
        Review::parse(&serde_json::to_vec(&valid).unwrap())
            .unwrap()
            .findings[0]
            .existing_comment_id,
        Some(42)
    );
    for (field, value) in [
        ("file", json!("../secrets")),
        ("line", json!(0)),
        ("severity", json!("critical")),
        ("message", json!("")),
    ] {
        let mut invalid = valid.clone();
        invalid["findings"][0][field] = value;
        assert!(Review::parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }
    assert!(Review::parse(
        &serde_json::to_vec(&json!({"findings":vec![valid["findings"][0].clone();11]})).unwrap()
    )
    .is_err());
}

#[tokio::test]
async fn github_discussion_fetches_all_pages_and_resolution_metadata() {
    async fn list(
        Path(path): Path<String>,
        Query(q): Query<HashMap<String, String>>,
    ) -> ([(String, String); 1], Json<Value>) {
        assert_eq!(q.get("per_page").map(String::as_str), Some("100"));
        let page = q["page"].parse::<u32>().unwrap();
        let next = if page == 1 {
            "<https://never-follow-provider-links.invalid>; rel=\"next\""
        } else {
            ""
        };
        (
            [("link".into(), next.into())],
            Json(json!([{"id":page,"body":path}])),
        )
    }
    async fn graphql(Json(q): Json<Value>) -> Json<Value> {
        assert_eq!(q["variables"]["owner"], "o");
        assert_eq!(q["variables"]["name"], "r");
        let first = q["variables"]["after"].is_null();
        Json(
            json!({"data":{"repository":{"pullRequest":{"reviewThreads":{
                "nodes":[{"isResolved":!first,"isOutdated":!first,"comments":{"nodes":[{"fullDatabaseId":if first {1}else{2}}]}}],
                "pageInfo":{"hasNextPage":first,"endCursor":"second"}
            }}}}}),
        )
    }
    let router = Router::new()
        .route("/graphql", post(graphql))
        .route("/repos/o/r/{*path}", get(list));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::new();
    let api = format!("http://{addr}/repos/o/r");
    let provider = Provider {
        http: &http,
        kind: "github",
        token: "test-only",
        api: &api,
    };
    let discussion = provider.discussion("1").await.unwrap();
    for key in ["comments", "reviews", "inline_comments", "threads"] {
        assert_eq!(discussion[key].as_array().unwrap().len(), 2);
    }
    assert_eq!(discussion["threads"][1]["isResolved"], true);
    server.abort();
}

#[tokio::test]
async fn provider_failures_are_explicit_and_never_expose_response_secrets() {
    let router = Router::new().route(
        "/repos/o/r/pulls/1",
        get(|| async {
            (
                axum::http::StatusCode::FORBIDDEN,
                "secret response must not escape",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/repos/o/r", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::new();
    let error = Provider {
        http: &http,
        kind: "github",
        token: "test-only",
        api: &api,
    }
    .metadata("1")
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("403"));
    assert!(!error.contains("secret response"));
    server.abort();
}
