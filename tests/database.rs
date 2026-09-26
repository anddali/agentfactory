//! Run explicitly against a disposable PostgreSQL database with DATABASE_URL set.
use anyhow::Result;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use chrono::Utc;
use factories::{
    api::{self, App, Identity},
    config::Platform,
    execution::Executor,
    model::*,
    storage::Blobs,
    store::Store,
};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;

async fn fixture() -> Result<Arc<App>> {
    let platform = Platform::load(Path::new("config/platform.yaml"))?;
    let snapshot = platform.snapshot(Path::new("workflows"), Path::new("prompts"))?;
    let store = Store::connect(&std::env::var("DATABASE_URL")?).await?;
    factories::releases::seed(&store, &platform, &snapshot).await?;
    Ok(Arc::new(App {
        store,
        platform,
        snapshot,
        local_configuration: false,
        executor: Executor {
            mode: "process".into(),
            server_url: "http://127.0.0.1:8080".into(),
            network: "unused".into(),
            worker_binary: env!("CARGO_BIN_EXE_factory-worker").into(),
            secret: "integration-worker-secret-32-characters".into(),
            ecs_cluster: String::new(),
            ecs_network: "{}".into(),
        },
        blobs: Blobs {
            root: std::env::temp_dir().join(format!("factory-test-{}", Uuid::new_v4())),
            bucket: None,
        },
        http: reqwest::Client::new(),
        identities: vec![
            Identity {
                token: "integration-test-operator-credential".into(),
                subject: "local-maintainer".into(),
                roles: vec!["operator".into(), "approver".into(), "observer".into()],
                repositories: vec!["local-demo".into()],
            },
            Identity {
                token: "integration-test-observer-credential".into(),
                subject: "observer".into(),
                roles: vec!["observer".into()],
                repositories: vec!["local-demo".into()],
            },
        ],
        public_read: false,
    }))
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn pr_link_submission_authorizes_derived_repository_before_provider_access() -> Result<()> {
    let app = fixture().await?;
    let router = api::router(app, "web");
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/pr-reviews")
                .header(
                    "Authorization",
                    "Bearer integration-test-operator-credential",
                )
                .header("Content-Type", "application/json")
                .header("Idempotency-Key", Uuid::new_v4().to_string())
                .body(Body::from(
                    json!({"pr_url":"https://github.com/outside/scope/pull/12"}).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn attention_dismissal_requires_operator_and_survives_reload() -> Result<()> {
    let app = fixture().await?;
    let job = app
        .submit(&Uuid::new_v4().to_string(), submission(), "test")
        .await?;
    let router = api::router(app.clone(), "web");
    for (credential, expected) in [
        (
            "integration-test-observer-credential",
            StatusCode::FORBIDDEN,
        ),
        ("integration-test-operator-credential", StatusCode::CONFLICT),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/jobs/{}/attention", job.id))
                    .header("Authorization", format!("Bearer {credential}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"dismissed":true}"#))?,
            )
            .await?;
        assert_eq!(response.status(), expected);
    }
    app.store
        .mutate(job.id, |j, c| j.cancel("test", Utc::now(), c))
        .await?;
    for dismissed in [true, true, false] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/jobs/{}/attention", job.id))
                    .header(
                        "Authorization",
                        "Bearer integration-test-operator-credential",
                    )
                    .header("Content-Type", "application/json")
                    .body(Body::from(json!({"dismissed":dismissed}).to_string()))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await?)?;
        assert_eq!(body["attention_dismissed"], dismissed);
        let persisted = app.store.job(job.id).await?;
        assert_eq!(persisted.attention_dismissed, dismissed);
        assert_eq!(persisted.status, JobStatus::Cancelled);
    }
    assert_eq!(
        app.store
            .events(Some(job.id))
            .await?
            .iter()
            .filter(|e| e.kind.starts_with("attention_"))
            .count(),
        2
    );
    Ok(())
}
fn submission() -> Submission {
    Submission {
        workflow: "demo".into(),
        repository: "local-demo".into(),
        issue: Issue {
            ticket: None,
            provider: "fixture".into(),
            key: Uuid::new_v4().to_string(),
            title: "Integration test".into(),
            body: "Test durable scheduling".into(),
            url: None,
        },
    }
}
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn concurrent_requests_commit_one_job_and_one_dispatch() -> Result<()> {
    let app = fixture().await?;
    let input = submission();
    let key = Uuid::new_v4().to_string();
    let (a, b) = tokio::join!(
        app.submit(&key, input.clone(), "test"),
        app.submit(&key, input.clone(), "test")
    );
    let a = a?;
    let b = b?;
    assert_eq!(a.id, b.id);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox WHERE job_id=$1 AND document->>'kind'='start'",
    )
    .bind(a.id)
    .fetch_one(&app.store.pool)
    .await?;
    assert_eq!(count, 1);
    let mut changed = input;
    changed.issue.title = "different".into();
    assert!(app.submit(&key, changed, "test").await.is_err());
    Ok(())
}
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn restarted_store_retains_gate_and_rejects_wrong_artifact() -> Result<()> {
    let app = fixture().await?;
    let job = app
        .submit(&Uuid::new_v4().to_string(), submission(), "test")
        .await?;
    let attempt = job.current().id;
    let instance = Uuid::new_v4();
    app.store
        .mutate(job.id, |j, c| {
            j.claim(attempt, instance, Utc::now(), c)?;
            j.current_mut().artifacts.insert(
                "research".into(),
                Artifact {
                    id: Uuid::new_v4(),
                    attempt_id: attempt,
                    name: "research".into(),
                    sha256: hash("research"),
                    size: 8,
                    created_at: Utc::now(),
                },
            );
            let result = Completion {
                agent_runs: vec![],
                succeeded: true,
                tasks: j
                    .phase()
                    .tasks
                    .iter()
                    .map(|t| TaskResult {
                        task: t.uses.clone(),
                        status: "succeeded".into(),
                        duration_ms: 2,
                        summary: "done".into(),
                    })
                    .collect(),
                findings: 0,
                pull_request: None,
                revision: None,
                error: None,
            };
            j.complete(attempt, instance, result, Utc::now(), c)
        })
        .await?;
    let restarted = Store::connect(&std::env::var("DATABASE_URL")?).await?;
    let loaded = restarted.job(job.id).await?;
    assert_eq!(loaded.status, JobStatus::AwaitingApproval);
    let gate = loaded.gates[0].clone();
    let mut decision = Decision {
        event_id: Uuid::new_v4().to_string(),
        gate_id: gate.id,
        artifact_digest: hash("wrong"),
        approve: true,
    };
    assert!(restarted
        .mutate(job.id, |j, c| j.decide(
            &decision,
            "local-maintainer",
            "api",
            Utc::now(),
            c
        ))
        .await
        .is_err());
    assert_eq!(
        restarted.job(job.id).await?.status,
        JobStatus::AwaitingApproval
    );
    decision.artifact_digest = gate.artifact_digest;
    restarted
        .mutate(job.id, |j, c| {
            j.decide(&decision, "local-maintainer", "api", Utc::now(), c)
        })
        .await?;
    assert_eq!(restarted.job(job.id).await?.attempts.len(), 2);
    Ok(())
}
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn observation_credentials_cannot_mutate_and_unknown_workers_cannot_claim() -> Result<()> {
    let app = fixture().await?;
    let router = api::router(app, "web");
    let unauthorized = router
        .clone()
        .oneshot(Request::builder().uri("/api/jobs").body(Body::empty())?)
        .await?;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let forbidden = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/jobs")
                .header(
                    "Authorization",
                    "Bearer integration-test-observer-credential",
                )
                .header("Content-Type", "application/json")
                .header("Idempotency-Key", Uuid::new_v4().to_string())
                .body(Body::from(serde_json::to_vec(&submission())?))?,
        )
        .await?;
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    let worker = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/worker/{}/claim", Uuid::new_v4()))
                .header(
                    "Authorization",
                    "Bearer integration-test-operator-credential",
                )
                .header("Content-Type", "application/json")
                .body(Body::from(json!({"instance":Uuid::new_v4()}).to_string()))?,
        )
        .await?;
    assert_eq!(worker.status(), StatusCode::UNAUTHORIZED);
    let body: Value = serde_json::from_slice(&to_bytes(worker.into_body(), 1024).await?)?;
    assert!(body["error"].as_str().unwrap().contains("attempt token"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn portal_access_reports_only_authorized_maintainer_repositories() -> Result<()> {
    let mut app = fixture().await?;
    Arc::get_mut(&mut app).unwrap().public_read = true;
    let router = api::router(app, "web");
    for (token, subject, repos) in [
        (None, Value::Null, json!([])),
        (
            Some("integration-test-observer-credential"),
            json!("observer"),
            json!([]),
        ),
        (
            Some("integration-test-operator-credential"),
            json!("local-maintainer"),
            json!(["local-demo"]),
        ),
    ] {
        let mut request = Request::builder().uri("/api/access");
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = router.clone().oneshot(request.body(Body::empty())?).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
        assert_eq!(body["subject"], subject);
        assert_eq!(body["approvable_repositories"], repos);
        assert!(!body.to_string().contains("credential"));
    }
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/access")
                .header("Authorization", "Bearer invalid")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn claimed_manifest_scopes_credentials_and_receipt_never_contains_them() -> Result<()> {
    let mut app = fixture().await?;
    let mutable = Arc::get_mut(&mut app).unwrap();
    let read_key = format!("TEST_REPO_READ_{}", Uuid::new_v4().simple());
    let write_key = format!("TEST_REPO_WRITE_{}", Uuid::new_v4().simple());
    std::env::set_var(&read_key, "integration-read-secret");
    std::env::set_var(&write_key, "integration-write-secret");
    let repo = mutable.platform.repositories.get_mut("local-demo").unwrap();
    repo.read_token_env = Some(read_key.clone());
    repo.write_token_env = Some(write_key.clone());
    let agent_key = format!("TEST_AGENT_{}", Uuid::new_v4().simple());
    std::env::set_var(&agent_key, "integration-agent-secret");
    let mut phase_agent = mutable.snapshot.agents["fixture"].clone();
    phase_agent.env_keys = vec![agent_key.clone()];
    phase_agent.version = 42;
    mutable
        .snapshot
        .agents
        .insert("phase-agent".into(), phase_agent);
    mutable.snapshot.workflows.get_mut("demo").unwrap().phases[0].agent_profile =
        Some("phase-agent".into());
    mutable.snapshot.workflows.get_mut("demo").unwrap().phases[0]
        .permissions
        .push("repository.read".into());
    mutable.platform.agents.insert(
        "phase-agent".into(),
        mutable.snapshot.agents["phase-agent"].clone(),
    );
    let root = format!("credential-test-{}", Uuid::new_v4().simple());
    let mut bundle = factories::releases::Bundle::from_snapshot(&mutable.snapshot, "demo")?;
    let mut workflow: factories::workflow::Workflow =
        serde_yaml::from_str(&bundle.workflows["demo"])?;
    workflow.id = root.clone();
    bundle.workflow = root.clone();
    bundle.workflows.clear();
    bundle
        .workflows
        .insert(root.clone(), serde_yaml::to_string(&workflow)?);
    let id = factories::releases::publish(
        &mutable.store,
        &mutable.platform,
        &bundle,
        "test",
        "Credential fixture",
    )
    .await?;
    factories::releases::activate(
        &mutable.store,
        &mutable.platform,
        id,
        0,
        "test",
        "Credential fixture",
    )
    .await?;
    let mut input = submission();
    input.workflow = root;
    let job = app
        .submit(&Uuid::new_v4().to_string(), input, "test")
        .await?;
    let router = api::router(app.clone(), "web");
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/worker/{}/claim", job.current().id))
                .header(
                    "Authorization",
                    format!(
                        "Bearer {}",
                        factories::execution::token(&app.executor.secret, job.current().id)
                    ),
                )
                .header("Content-Type", "application/json")
                .body(Body::from(json!({"instance":Uuid::new_v4()}).to_string()))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("Cache-Control").unwrap(), "no-store");
    let manifest: Value = serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await?)?;
    assert_eq!(
        manifest["credentials"]["repository_token"],
        "integration-read-secret"
    );
    assert!(!manifest.to_string().contains("integration-write-secret"));
    assert_eq!(manifest["agent"]["version"], 42);
    assert_eq!(
        manifest["credentials"]["agent_env"][&agent_key],
        "integration-agent-secret"
    );
    let receipt = router
        .oneshot(
            Request::builder()
                .uri(format!("/api/jobs/{}/receipt", job.id))
                .header(
                    "Authorization",
                    "Bearer integration-test-observer-credential",
                )
                .body(Body::empty())?,
        )
        .await?;
    let text = String::from_utf8(to_bytes(receipt.into_body(), 100_000).await?.to_vec())?;
    assert!(
        !text.contains("integration-read-secret") && !text.contains("integration-write-secret")
    );
    assert!(!text.contains("integration-agent-secret"));
    std::env::remove_var(read_key);
    std::env::remove_var(write_key);
    std::env::remove_var(agent_key);
    Ok(())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn connector_configuration_is_admin_only_encrypted_and_versioned() -> Result<()> {
    use factories::connectors;
    let mut app = fixture().await?;
    Arc::get_mut(&mut app).unwrap().identities[0]
        .roles
        .push("connector_admin".into());
    sqlx::query("DELETE FROM connectors WHERE kind='jira'")
        .execute(&app.store.pool)
        .await?;
    let router = api::router(app.clone(), "web");
    for credential in [None, Some("integration-test-observer-credential")] {
        for method in ["GET", "PUT"] {
            let path = if method == "GET" {
                "/api/connectors"
            } else {
                "/api/connectors/jira"
            };
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("Content-Type", "application/json");
            if let Some(token) = credential {
                request = request.header("Authorization", format!("Bearer {token}"));
            }
            let body = if method == "GET" {
                String::new()
            } else {
                json!({"revision":0,"enabled":false,"repositories":[],"values":{}}).to_string()
            };
            let response = router
                .clone()
                .oneshot(request.body(Body::from(body))?)
                .await?;
            assert!(matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ));
        }
    }
    let unauth_test = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/connectors/jira/test")
                .header(
                    "Authorization",
                    "Bearer integration-test-observer-credential",
                )
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({"revision":0,"enabled":false,"repositories":[],"values":{}}).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(unauth_test.status(), StatusCode::FORBIDDEN);
    let admin = "integration-test-operator-credential";
    let input = json!({"revision":0,"enabled":true,"repositories":["local-demo"],"values":{"base_url":"https://example.atlassian.net","email":"test@example.test","token":"sensitive-test-value"}});
    let request = |body: Value| {
        Request::builder()
            .method("PUT")
            .uri("/api/connectors/jira")
            .header("Authorization", format!("Bearer {admin}"))
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    assert_eq!(
        router
            .clone()
            .oneshot(request(input.clone()))
            .await?
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        router.clone().oneshot(request(input)).await?.status(),
        StatusCode::CONFLICT
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/connectors")
                .header("Authorization", format!("Bearer {admin}"))
                .body(Body::empty())?,
        )
        .await?;
    let body = to_bytes(response.into_body(), 100_000).await?;
    assert!(!String::from_utf8_lossy(&body).contains("sensitive-test-value"));
    let list: Value = serde_json::from_slice(&body)?;
    let jira = list["connectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["definition"]["kind"] == "jira")
        .unwrap();
    assert_eq!(jira["secrets"]["token"], true);
    let ciphertext: Vec<u8> =
        sqlx::query_scalar("SELECT ciphertext FROM connectors WHERE kind='jira'")
            .fetch_one(&app.store.pool)
            .await?;
    assert!(!ciphertext.windows(20).any(|w| w == b"sensitive-test-value"));
    let next = json!({"revision":1,"enabled":false,"repositories":["local-demo"],"values":{"base_url":"https://example.atlassian.net","email":"changed@example.test","token":""}});
    assert_eq!(
        router.clone().oneshot(request(next)).await?.status(),
        StatusCode::OK
    );
    let c = connectors::load(&app.store, &app.executor.secret, "jira")
        .await?
        .unwrap();
    assert_eq!(c.value("token")?, "sensitive-test-value");
    assert!(c.authorize("local-demo").is_err());
    assert!(connectors::credential(
        &app.store,
        &app.executor.secret,
        "jira",
        "token",
        "UNUSED_ENV"
    )
    .await
    .is_err());
    let clear = json!({"revision":2,"enabled":false,"repositories":[],"values":{},"clear_secrets":["token"]});
    assert_eq!(
        router.clone().oneshot(request(clear)).await?.status(),
        StatusCode::OK
    );
    assert!(connectors::load(&app.store, &app.executor.secret, "jira")
        .await?
        .unwrap()
        .value("token")
        .is_err());
    let test_response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/connectors/jira/test")
                .header("Authorization", format!("Bearer {admin}"))
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({"revision":3,"enabled":true,"repositories":[],"values":{}}).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(test_response.status(), StatusCode::OK);
    let report: Value =
        serde_json::from_slice(&to_bytes(test_response.into_body(), 100_000).await?)?;
    assert_eq!(report["ok"], false);
    assert_eq!(report["saved"], false);
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM connectors WHERE kind='jira'")
        .fetch_one(&app.store.pool)
        .await?;
    assert_eq!(revision, 3);
    assert!(
        !connectors::load(&app.store, &app.executor.secret, "jira")
            .await?
            .unwrap()
            .enabled
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn portal_repository_credentials_enforce_scope_and_read_write_selection() -> Result<()> {
    use factories::connectors::{self, Update};
    let app = fixture().await?;
    let mut platform = app.platform.clone();
    let repo = platform.repositories.get_mut("local-demo").unwrap();
    repo.provider = "github".into();
    repo.url = "https://github.com/team/repo.git".into();
    repo.api_url = "https://api.github.com/repos/team/repo".into();
    let repo = repo.clone();
    sqlx::query("DELETE FROM connectors WHERE kind='github'")
        .execute(&app.store.pool)
        .await?;
    let update: Update = serde_json::from_value(
        json!({"revision":0,"enabled":true,"repositories":["local-demo"],"values":{"base_url":"https://github.com","api_url":"https://api.github.com","token":"read-test","write_token":"write-test"}}),
    )?;
    connectors::save(
        &app.store,
        &app.executor.secret,
        &platform,
        "github",
        update,
        "test-admin",
    )
    .await?;
    assert_eq!(
        connectors::repository_token(&app.store, &app.executor.secret, "local-demo", &repo, false)
            .await?
            .as_deref(),
        Some("read-test")
    );
    assert_eq!(
        connectors::repository_token(&app.store, &app.executor.secret, "local-demo", &repo, true)
            .await?
            .as_deref(),
        Some("write-test")
    );
    assert!(connectors::repository_token(
        &app.store,
        &app.executor.secret,
        "other-repository",
        &repo,
        true
    )
    .await
    .is_ok());
    // A job pinned from a URL must remain claimable without any YAML alias.
    let mut dynamic = (*app).clone();
    dynamic.platform.repositories.clear();
    dynamic.identities[0].repositories = vec!["*".into()];
    let mut input = submission();
    input.repository = "https://github.com/team/new-repository".into();
    let mut snapshot = dynamic.snapshot.clone();
    snapshot.workflows.get_mut("demo").unwrap().phases[0]
        .permissions
        .push("repository.read".into());
    snapshot.refresh_hash()?;
    let pinned = Repository {
        id: input.repository.clone(),
        url: input.repository.clone(),
        api_url: "https://api.github.com/repos/team/new-repository".into(),
        provider: "github".into(),
        revision: "a".repeat(40),
        base_branch: "main".into(),
        work_branch: String::new(),
    };
    let job = dynamic
        .store
        .submit(
            &Uuid::new_v4().to_string(),
            &hash(serde_json::to_vec(&input)?),
            input,
            pinned,
            snapshot,
            "test",
        )
        .await?;
    let router = api::router(Arc::new(dynamic), "web");
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/worker/{}/claim", job.current().id))
                .header(
                    "Authorization",
                    format!(
                        "Bearer {}",
                        factories::execution::token(&app.executor.secret, job.current().id)
                    ),
                )
                .header("Content-Type", "application/json")
                .body(Body::from(json!({"instance":Uuid::new_v4()}).to_string()))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await?)?;
    assert_eq!(body["credentials"]["repository_token"], "read-test");
    assert_eq!(
        body["repository"]["id"],
        "https://github.com/team/new-repository"
    );
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/access")
                .header(
                    "Authorization",
                    "Bearer integration-test-operator-credential",
                )
                .body(Body::empty())?,
        )
        .await?;
    let body: Value = serde_json::from_slice(&to_bytes(response.into_body(), 4096).await?)?;
    assert_eq!(body["operable_repositories"], json!(["*"]));
    assert_eq!(body["dynamic_approvable_repositories"], json!(["*"]));
    let mut changed = repo;
    changed.api_url = "https://attacker.example/repos/team/repo".into();
    assert!(connectors::repository_token(
        &app.store,
        &app.executor.secret,
        "local-demo",
        &changed,
        false
    )
    .await
    .is_err());
    Ok(())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn slack_uses_default_channel_without_repository_registration_or_selection() -> Result<()> {
    use factories::connectors::{self, Update};
    let app = fixture().await?;
    sqlx::query("DELETE FROM connectors WHERE kind='slack'")
        .execute(&app.store.pool)
        .await?;
    let update: Update = serde_json::from_value(
        json!({"revision":0,"enabled":true,"values":{"token":"test-bot","signing_secret":"test-signing","channel":"C_DEFAULT"}}),
    )?;
    connectors::save(
        &app.store,
        &app.executor.secret,
        &app.platform,
        "slack",
        update,
        "test-admin",
    )
    .await?;
    assert_eq!(
        connectors::slack_channel(
            &app.store,
            &app.executor.secret,
            &app.platform,
            "https://github.com/team/new"
        )
        .await?,
        "C_DEFAULT"
    );
    let update: Update = serde_json::from_value(json!({"revision":1,"enabled":false,"values":{}}))?;
    connectors::save(
        &app.store,
        &app.executor.secret,
        &app.platform,
        "slack",
        update,
        "test-admin",
    )
    .await?;
    assert!(connectors::slack_channel(
        &app.store,
        &app.executor.secret,
        &app.platform,
        "https://github.com/team/new"
    )
    .await
    .is_err());
    Ok(())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn release_registry_conflicts_rollback_and_job_pinning() -> Result<()> {
    use factories::releases::{self, Bundle};
    let app = fixture().await?;
    let root = format!("release-test-{}", Uuid::new_v4().simple());
    let mut bundle = Bundle::from_snapshot(&app.snapshot, "demo")?;
    let mut w: factories::workflow::Workflow = serde_yaml::from_str(&bundle.workflows["demo"])?;
    w.id = root.clone();
    bundle.workflow = root.clone();
    bundle.workflows.clear();
    bundle
        .workflows
        .insert(root.clone(), serde_yaml::to_string(&w)?);
    let first = releases::publish(
        &app.store,
        &app.platform,
        &bundle,
        "test",
        "Initial candidate",
    )
    .await?;
    assert_eq!(
        first,
        releases::publish(&app.store, &app.platform, &bundle, "test", "Same import").await?
    );
    assert!(releases::active(&app.store, &app.platform, &root)
        .await
        .is_err());
    releases::activate(
        &app.store,
        &app.platform,
        first,
        0,
        "test",
        "Activate original",
    )
    .await?;
    let mut input = submission();
    input.workflow = root.clone();
    let key = Uuid::new_v4().to_string();
    let job = app.submit(&key, input.clone(), "test").await?;
    assert_eq!(job.snapshot.release_id, Some(first));
    bundle.base_generation = 1;
    w.version += 1;
    w.description = "Updated release".into();
    bundle
        .workflows
        .insert(root.clone(), serde_yaml::to_string(&w)?);
    let second = releases::publish(
        &app.store,
        &app.platform,
        &bundle,
        "test",
        "Changed description",
    )
    .await?;
    let (a, b) = tokio::join!(
        releases::activate(&app.store, &app.platform, second, 1, "test", "Concurrent A"),
        releases::activate(&app.store, &app.platform, second, 1, "test", "Concurrent B")
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let same = app.submit(&key, input.clone(), "test").await?;
    assert_eq!(same.id, job.id);
    assert_eq!(same.snapshot.release_id, Some(first));
    let new = app
        .submit(&Uuid::new_v4().to_string(), input.clone(), "test")
        .await?;
    assert_eq!(new.snapshot.release_id, Some(second));
    assert!(
        releases::publish(&app.store, &app.platform, &bundle, "test", "Stale base")
            .await
            .is_err()
    );
    releases::activate(
        &app.store,
        &app.platform,
        first,
        2,
        "test",
        "Restore original",
    )
    .await?;
    let restored = releases::active(&app.store, &app.platform, &root).await?;
    assert_eq!(restored.release_id, Some(first));
    assert_eq!(
        app.store.job(new.id).await?.snapshot.release_id,
        Some(second)
    );
    bundle.base_generation = 3;
    w.description = "Illegal overwrite".into();
    bundle
        .workflows
        .insert(root.clone(), serde_yaml::to_string(&w)?);
    assert!(releases::publish(
        &app.store,
        &app.platform,
        &bundle,
        "test",
        "Changed immutable version"
    )
    .await
    .is_err());
    let restored_store = Store::connect(&std::env::var("DATABASE_URL")?).await?;
    releases::seed(&restored_store, &app.platform, &app.snapshot).await?;
    assert_eq!(
        releases::active(&restored_store, &app.platform, &root)
            .await?
            .release_id,
        Some(first)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn configuration_authority_draft_conflicts_and_validation() -> Result<()> {
    let mut app = fixture().await?;
    let a = Arc::make_mut(&mut app);
    a.identities.push(Identity {
        token: "configuration-editor-credential".into(),
        subject: "editor".into(),
        roles: vec!["configuration_editor".into()],
        repositories: vec![],
    });
    a.identities.push(Identity {
        token: "configuration-publisher-credential".into(),
        subject: "publisher".into(),
        roles: vec!["configuration_publisher".into()],
        repositories: vec![],
    });
    let router = api::router(app.clone(), "web");
    let bundle = factories::releases::Bundle::from_snapshot(&app.snapshot, "demo")?;
    let draft = Uuid::new_v4();
    let request = |method: &str, path: String, token: &str, body: Value| {
        Request::builder()
            .method(method)
            .uri(path)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    for token in [
        "integration-test-observer-credential",
        "configuration-publisher-credential",
    ] {
        let r = router
            .clone()
            .oneshot(request(
                "PUT",
                format!("/api/configuration/drafts/{draft}"),
                token,
                json!({"revision":0,"bundle":bundle}),
            ))
            .await?;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
    for (revision, expected) in [
        (0, StatusCode::OK),
        (0, StatusCode::CONFLICT),
        (1, StatusCode::OK),
        (1, StatusCode::CONFLICT),
    ] {
        let r = router
            .clone()
            .oneshot(request(
                "PUT",
                format!("/api/configuration/drafts/{draft}"),
                "configuration-editor-credential",
                json!({"revision":revision,"bundle":bundle}),
            ))
            .await?;
        assert_eq!(r.status(), expected);
    }
    let r = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/configuration/releases".into(),
            "configuration-editor-credential",
            json!({"bundle":bundle,"note":"Not authorized"}),
        ))
        .await?;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let mut invalid = factories::releases::Bundle::from_snapshot(&app.snapshot, "research-plan")?;
    invalid.prompts.clear();
    let r = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/configuration/validate".into(),
            "configuration-editor-credential",
            serde_json::to_value(invalid)?,
        ))
        .await?;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    Ok(())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn local_files_mode_uses_local_snapshot_and_disables_portal_mutations() -> Result<()> {
    let mut app = fixture().await?;
    let mutable = Arc::make_mut(&mut app);
    mutable.local_configuration = true;
    mutable.snapshot.workflows.get_mut("demo").unwrap().version = 999;
    mutable.identities[0]
        .roles
        .push("configuration_editor".into());
    let job = app
        .submit(&Uuid::new_v4().to_string(), submission(), "test")
        .await?;
    assert_eq!(job.snapshot.workflows["demo"].version, 999);
    assert!(job.snapshot.release_id.is_none());
    assert_ne!(
        factories::releases::active(&app.store, &app.platform, "demo")
            .await?
            .workflows["demo"]
            .version,
        999
    );
    let bundle = factories::releases::Bundle::from_snapshot(&app.snapshot, "demo")?;
    let response = api::router(app, "web")
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/configuration/validate")
                .header(
                    "Authorization",
                    "Bearer integration-test-operator-credential",
                )
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&bundle)?))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    Ok(())
}

async fn design_fixture(app: &App) -> Result<factories::releases::Bundle> {
    let root = format!("design-{}", Uuid::new_v4().simple());
    let mut bundle = factories::releases::Bundle::from_snapshot(&app.snapshot, "demo")?;
    let mut w: factories::workflow::Workflow = serde_yaml::from_str(&bundle.workflows["demo"])?;
    w.id = root.clone();
    w.phases.truncate(1);
    w.phases[0].gate = None;
    let prompt = format!("{root}@1");
    for t in &mut w.phases[0].tasks {
        if t.with.contains_key("prompt") {
            t.with.insert("prompt".into(), prompt.clone());
        }
    }
    bundle.workflow = root.clone();
    bundle.workflows.clear();
    bundle.workflows.insert(root, serde_yaml::to_string(&w)?);
    bundle.prompts.clear();
    bundle
        .prompts
        .insert(prompt, "Original instructions".into());
    let release =
        factories::releases::publish(&app.store, &app.platform, &bundle, "test", "Design fixture")
            .await?;
    factories::releases::activate(
        &app.store,
        &app.platform,
        release,
        0,
        "test",
        "Design fixture",
    )
    .await?;
    bundle.base_generation = 1;
    Ok(bundle)
}
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn design_prepares_versions_and_atomically_publishes_saved_candidate() -> Result<()> {
    use factories::releases;
    let app = fixture().await?;
    let mut bundle = design_fixture(&app).await?;
    let original_name = bundle.prompts.keys().next().unwrap().clone();
    bundle
        .prompts
        .insert(original_name.clone(), "Revised instructions".into());
    let prepared = releases::prepare(&app.store, &app.platform, bundle).await?;
    let next = format!("{}@2", prepared.workflow);
    assert_eq!(prepared.prompts[&next], "Revised instructions");
    let w: factories::workflow::Workflow =
        serde_yaml::from_str(&prepared.workflows[&prepared.workflow])?;
    assert_eq!(w.version, 2);
    assert!(w.phases[0]
        .tasks
        .iter()
        .any(|t| t.with.get("prompt") == Some(&next)));
    let again = releases::prepare(&app.store, &app.platform, prepared.clone()).await?;
    assert_eq!(prepared.digest()?, again.digest()?);
    let draft = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO configuration_drafts(id,revision,bundle,actor) VALUES($1,1,$2,'test')",
    )
    .bind(draft)
    .bind(serde_json::to_value(&prepared)?)
    .execute(&app.store.pool)
    .await?;
    let mut unsaved = prepared.clone();
    unsaved.prompts.insert(next, "Unsaved edit".into());
    assert!(releases::publish_with_options(
        &app.store,
        &app.platform,
        &unsaved,
        "test",
        "Wrong candidate",
        Some((draft, 1)),
        true
    )
    .await
    .is_err());
    let release = releases::publish_with_options(
        &app.store,
        &app.platform,
        &prepared,
        "test",
        "Reviewed candidate",
        Some((draft, 1)),
        true,
    )
    .await?;
    assert_eq!(
        releases::active(&app.store, &app.platform, &prepared.workflow)
            .await?
            .release_id,
        Some(release)
    );
    let status: String = sqlx::query_scalar("SELECT status FROM configuration_drafts WHERE id=$1")
        .bind(draft)
        .fetch_one(&app.store.pool)
        .await?;
    assert_eq!(status, "published");
    let number: i64 =
        sqlx::query_scalar("SELECT release_number FROM workflow_releases WHERE id=$1")
            .bind(release)
            .fetch_one(&app.store.pool)
            .await?;
    assert_eq!(number, 2);
    Ok(())
}
#[tokio::test]
#[ignore = "requires DATABASE_URL pointing at a disposable PostgreSQL database"]
async fn prompt_adoption_creates_only_selected_drafts_and_tracks_test_candidate() -> Result<()> {
    let mut app = fixture().await?;
    let mutable = Arc::make_mut(&mut app);
    mutable.identities[0].roles.extend([
        "configuration_editor".into(),
        "configuration_publisher".into(),
    ]);
    let original = design_fixture(&app).await?;
    let router = api::router(app.clone(), "web");
    let request = |path: &str, body: Value| {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(
                "Authorization",
                "Bearer integration-test-operator-credential",
            )
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let input = json!({"name":original.workflow,"content":"Shared prompt update","note":"Test adoption","expected_name":format!("{}@1",original.workflow),"consumers":[{"workflow":original.workflow,"expected_generation":1}]});
    let response = router
        .clone()
        .oneshot(request("/api/configuration/prompts/revise", input.clone()))
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value = serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await?)?;
    assert_eq!(result["name"], format!("{}@2", original.workflow));
    let draft = result["drafts"][0]["id"].as_str().unwrap();
    let value: Value = sqlx::query_scalar("SELECT bundle FROM configuration_drafts WHERE id=$1")
        .bind(Uuid::parse_str(draft)?)
        .fetch_one(&app.store.pool)
        .await?;
    let candidate: factories::releases::Bundle = serde_json::from_value(value)?;
    assert!(candidate
        .prompts
        .values()
        .any(|p| p == "Shared prompt update"));
    let active = factories::releases::active(&app.store, &app.platform, &original.workflow).await?;
    assert!(active
        .prompts
        .values()
        .any(|p| p.text == "Original instructions"));
    let stale = router
        .clone()
        .oneshot(request("/api/configuration/prompts/revise", input))
        .await?;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let mut submission = submission();
    submission.workflow = original.workflow.clone();
    let test = router
        .clone()
        .oneshot(request(
            &format!("/api/configuration/drafts/{draft}/test"),
            json!({"revision":1,"submission":submission,"key":"test-1"}),
        ))
        .await?;
    assert_eq!(test.status(), StatusCode::OK);
    let test_result: Value = serde_json::from_slice(&to_bytes(test.into_body(), 100_000).await?)?;
    let digest: String =
        sqlx::query_scalar("SELECT candidate_digest FROM configuration_tests WHERE job_id=$1")
            .bind(Uuid::parse_str(test_result["job_id"].as_str().unwrap())?)
            .fetch_one(&app.store.pool)
            .await?;
    assert_eq!(digest, candidate.digest()?);
    let discard = router
        .clone()
        .oneshot(request(
            &format!("/api/configuration/drafts/{draft}/discard"),
            json!({"revision":1}),
        ))
        .await?;
    assert_eq!(discard.status(), StatusCode::OK);
    let no_test = router
        .clone()
        .oneshot(request(
            &format!("/api/configuration/drafts/{draft}/test"),
            json!({"revision":1,"submission":submission,"key":"test-2"}),
        ))
        .await?;
    assert_eq!(no_test.status(), StatusCode::CONFLICT);
    Ok(())
}
