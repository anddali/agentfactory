use super::*;
use axum::{body::to_bytes, extract::Path, routing::post, Router};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::future::IntoFuture;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

#[derive(Clone, Default)]
struct Mock {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    fail: Arc<AtomicBool>,
    fail_thread: Arc<AtomicBool>,
}
async fn slack_api(
    State(mock): State<Mock>,
    Path(method): Path<String>,
    Json(value): Json<Value>,
) -> Json<Value> {
    let thread = method == "chat.postMessage" && !value["thread_ts"].is_null();
    mock.calls.lock().unwrap().push((method.clone(), value));
    if mock.fail.load(Ordering::SeqCst) || (thread && mock.fail_thread.load(Ordering::SeqCst)) {
        return Json(json!({"ok":false,"error":"ratelimited"}));
    }
    Json(match method.as_str() {
        "views.open" => json!({"ok":true,"view":{"id":format!("V{}",Uuid::new_v4())}}),
        "chat.postMessage" => json!({"ok":true,"ts":"123.456"}),
        _ => json!({"ok":true}),
    })
}
async fn signed(app: Arc<App>, values: &[(&str, String)]) -> (StatusCode, Value) {
    let body = reqwest::Url::parse_with_params("https://test.invalid", values)
        .unwrap()
        .query()
        .unwrap()
        .to_owned();
    let stamp = Utc::now().timestamp().to_string();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"test-signing-secret").unwrap();
    mac.update(format!("v0:{stamp}:{body}").as_bytes());
    let mut headers = HeaderMap::new();
    headers.insert("X-Slack-Request-Timestamp", stamp.parse().unwrap());
    headers.insert(
        "X-Slack-Signature",
        format!("v0={}", hex::encode(mac.finalize().into_bytes()))
            .parse()
            .unwrap(),
    );
    let response = hook(State(app), headers, Bytes::from(body)).await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 2_000_000).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
async fn click(app: &Arc<App>, payload: Value) -> Value {
    let (status, v) = signed(app.clone(), &[("payload", payload.to_string())]).await;
    assert_eq!(status, StatusCode::OK);
    v
}
async fn session(app: &App, id: Uuid) -> (String, String, Value) {
    sqlx::query_as("SELECT stage,view_id,data FROM slack_sessions WHERE id=$1")
        .bind(id)
        .fetch_one(&app.store.pool)
        .await
        .unwrap()
}
fn submit(id: Uuid, view: &str, callback: &str, values: Value) -> Value {
    json!({"type":"view_submission","team":{"id":"T_TEST"},"user":{"id":"U_ADMIN"},"view":{"id":view,"private_metadata":id,"callback_id":callback,"state":{"values":values}}})
}
fn choice(value: &str) -> Value {
    json!({"value":{"selected_option":{"value":value}}})
}
fn text(value: &str) -> Value {
    json!({"value":{"value":value}})
}
async fn finish_phase(app: &App, id: Uuid) -> Job {
    let report =
        b"# Test report\nA real, hash-verified fixture report. <!channel> must remain plain text.";
    let digest = app.blobs.put(report).await.unwrap();
    app.store
        .mutate(id, |j, c| {
            let attempt = j.current().id;
            let instance = Uuid::new_v4();
            let now = Utc::now();
            j.claim(attempt, instance, now, c)?;
            let names: Vec<_> = j
                .phase()
                .tasks
                .iter()
                .filter(|t| t.uses == "artifact.publish")
                .map(|t| t.with["name"].clone())
                .collect();
            for name in names {
                j.current_mut().artifacts.insert(
                    name.clone(),
                    Artifact {
                        id: Uuid::new_v4(),
                        attempt_id: attempt,
                        name,
                        sha256: digest.clone(),
                        size: report.len(),
                        created_at: now,
                    },
                );
            }
            let tasks = j
                .phase()
                .tasks
                .iter()
                .map(|t| TaskResult {
                    task: t.uses.clone(),
                    status: "succeeded".into(),
                    duration_ms: 1,
                    summary: "done".into(),
                })
                .collect();
            j.complete(
                attempt,
                instance,
                Completion {
                    agent_runs: vec![],
                    succeeded: true,
                    tasks,
                    findings: 0,
                    pull_request: None,
                    revision: None,
                    error: None,
                },
                now,
                c,
            )
        })
        .await
        .unwrap()
}
async fn fresh_approval(app: &Arc<App>, mock: &Mock, gate: Uuid) -> Uuid {
    let result=click(app,json!({"type":"block_actions","team":{"id":"T_TEST"},"user":{"id":"U_ADMIN"},"channel":{"id":"C_TEST"},"trigger_id":"trigger","actions":[{"action_id":"factory_review_gate","value":gate}]})).await;
    assert_eq!(result, json!({}));
    let id: Uuid = mock
        .calls
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(m, _)| m == "views.open")
        .unwrap()
        .1["view"]["private_metadata"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(process_one(app).await.unwrap());
    assert_eq!(session(app, id).await.0, "approval");
    id
}
async fn due(app: &App) {
    sqlx::query("UPDATE slack_messages SET available_at=now()")
        .execute(&app.store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE slack_sessions SET available_at=now() WHERE stage='render_pending'")
        .execute(&app.store.pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires DATABASE_URL for a disposable PostgreSQL database; uses a local mock Slack API"]
async fn complete_journey_permissions_replay_failures_and_followups() -> Result<()> {
    let mock = Mock::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    TEST_SLACK_URL
        .set(format!("http://{}", listener.local_addr()?))
        .unwrap();
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/{method}", post(slack_api))
                .with_state(mock.clone()),
        )
        .into_future(),
    );
    let mut platform = crate::config::Platform::load(std::path::Path::new("config/platform.yaml"))?;
    platform
        .repositories
        .get_mut("local-demo")
        .unwrap()
        .maintainers = vec!["slack:U_ADMIN".into()];
    let mut snapshot = platform.snapshot(
        std::path::Path::new("workflows"),
        std::path::Path::new("prompts"),
    )?;
    for phase in &mut snapshot.workflows.get_mut("demo").unwrap().phases {
        if let Some(g) = &mut phase.gate {
            g.channels.push("slack".into());
        }
    }
    snapshot.refresh_hash()?;
    let store = crate::store::Store::connect(&std::env::var("DATABASE_URL")?).await?;
    let temp = tempfile::tempdir()?;
    let app = Arc::new(App {
        store,
        platform,
        snapshot,
        local_configuration: true,
        executor: crate::execution::Executor {
            mode: "process".into(),
            server_url: "unused".into(),
            network: "unused".into(),
            worker_binary: std::env::current_exe()?,
            secret: "test-slack-master-secret-32-characters".into(),
            ecs_cluster: String::new(),
            ecs_network: String::new(),
        },
        blobs: crate::storage::Blobs {
            root: temp.path().into(),
            bucket: None,
        },
        http: reqwest::Client::new(),
        identities: vec![
            Identity {
                token: "unused-test-api-token-24-chars".into(),
                subject: "slack:U_ADMIN".into(),
                roles: vec!["operator".into(), "approver".into(), "observer".into()],
                repositories: vec!["local-demo".into()],
            },
            Identity {
                token: "unused-observer-token-24-chars".into(),
                subject: "slack:U_OBSERVER".into(),
                roles: vec!["observer".into()],
                repositories: vec!["local-demo".into()],
            },
        ],
        public_read: false,
    });
    let revision: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM connectors WHERE kind='slack'")
            .fetch_optional(&app.store.pool)
            .await?;
    crate::connectors::save(
        &app.store,
        &app.executor.secret,
        &app.platform,
        "slack",
        crate::connectors::Update {
            revision: revision.unwrap_or(0),
            enabled: true,
            repositories: vec![],
            clear_secrets: vec![],
            values: BTreeMap::from([
                ("token".into(), "test-bot-token".into()),
                ("signing_secret".into(), "test-signing-secret".into()),
                ("team_id".into(), "T_TEST".into()),
                ("channel".into(), "C_TEST".into()),
            ]),
        },
        "test",
    )
    .await?;
    let bad = hook(
        State(app.clone()),
        HeaderMap::new(),
        Bytes::from("bad signature"),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
    let params = |team: &str, user: &str| {
        vec![
            ("team_id", team.into()),
            ("user_id", user.into()),
            ("channel_id", "C_TEST".into()),
            ("trigger_id", "trigger".into()),
            ("text", "".into()),
        ]
    };
    let (_, wrong) = signed(app.clone(), &params("T_WRONG", "U_ADMIN")).await;
    assert!(wrong["text"].as_str().unwrap().contains("workspace"));
    let (_, denied) = signed(app.clone(), &params("T_TEST", "U_OBSERVER")).await;
    assert!(denied["text"].as_str().unwrap().contains("Unable"));
    assert!(mock.calls.lock().unwrap().is_empty());
    signed(app.clone(), &params("T_TEST", "U_ADMIN")).await;
    let id: Uuid = mock.calls.lock().unwrap().last().unwrap().1["view"]["private_metadata"]
        .as_str()
        .unwrap()
        .parse()?;
    let (_, view, _) = session(&app, id).await;
    let chosen = click(
        &app,
        submit(
            id,
            &view,
            "factory_choose",
            json!({"workflow":choice("demo")}),
        ),
    )
    .await;
    assert_eq!(chosen["view"]["callback_id"], "factory_input");
    let denied = click(
        &app,
        submit(
            id,
            &view,
            "factory_input",
            json!({"reference":text("TEST-1"),"repository":choice("forbidden")}),
        ),
    )
    .await;
    assert!(denied["errors"]["repository"].is_string());
    let request = submit(
        id,
        &view,
        "factory_input",
        json!({"reference":text("TEST-1"),"repository":choice("local-demo")}),
    );
    click(&app, request).await;
    assert!(process_one(&app).await?);
    assert_eq!(session(&app, id).await.0, "preview");
    let preview = session(&app, id).await.2;
    assert_eq!(preview["submission"]["issue"]["title"], "TEST-1");
    let confirm = submit(
        id,
        &view,
        "factory_confirm",
        json!({"confirm":choice("start")}),
    );
    let (a, b) = tokio::join!(click(&app, confirm.clone()), click(&app, confirm.clone()));
    assert_eq!(a["response_action"], "update");
    assert_eq!(b["response_action"], "update");
    // Reconstruct the worker after acceptance: all required state is in PostgreSQL.
    let resumed = Arc::new((*app).clone());
    assert!(process_one(&resumed).await?);
    assert_eq!(session(&app, id).await.0, "started");
    let job_id: Uuid = serde_json::from_value(session(&app, id).await.2["job"].clone())?;
    click(&app, confirm).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM requests WHERE request_key=$1")
        .bind(format!("slack:U_ADMIN:slack:{id}"))
        .fetch_one(&app.store.pool)
        .await?;
    assert_eq!(count, 1);
    let job = finish_phase(&app, job_id).await;
    let gate = job.gates[0].id;
    mock.fail_thread.store(true, Ordering::SeqCst);
    assert!(sync_one(&app).await?);
    let (ts, milestone, tries): (Option<String>, Option<String>, i32) = sqlx::query_as(
        "SELECT message_ts,milestone_hash,tries FROM slack_messages WHERE root_id=$1",
    )
    .bind(job_id)
    .fetch_one(&app.store.pool)
    .await?;
    assert_eq!(ts.as_deref(), Some("123.456"));
    assert!(milestone.is_none());
    assert_eq!(tries, 1);
    mock.fail_thread.store(false, Ordering::SeqCst);
    due(&app).await;
    assert!(sync_one(&app).await?);
    let (milestone, tries): (Option<String>, i32) = sqlx::query_as(
        "SELECT milestone_hash,tries FROM slack_messages WHERE root_id=$1",
    )
    .bind(job_id)
    .fetch_one(&app.store.pool)
    .await?;
    assert!(milestone.is_some());
    assert_eq!(tries, 0);
    {
        let calls = mock.calls.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|(m, v)| m == "chat.postMessage" && v["thread_ts"].is_null()).count(),
            1
        );
        assert_eq!(
            calls.iter().filter(|(m, v)| m == "chat.postMessage" && v["thread_ts"] == "123.456").count(),
            2
        );
    }
    let first = fresh_approval(&app, &mock, gate).await;
    let second = fresh_approval(&app, &mock, gate).await;
    let (_, first_view, _) = session(&app, first).await;
    let (_, second_view, _) = session(&app, second).await;
    let mut unauthorized = submit(
        first,
        &first_view,
        "factory_approval",
        json!({"decision":choice("approve")}),
    );
    unauthorized["user"]["id"] = json!("U_OBSERVER");
    assert_eq!(click(&app, unauthorized).await["response_action"], "errors");
    click(
        &app,
        submit(
            first,
            &first_view,
            "factory_approval",
            json!({"decision":choice("approve")}),
        ),
    )
    .await;
    let stale = click(
        &app,
        submit(
            second,
            &second_view,
            "factory_approval",
            json!({"decision":choice("reject")}),
        ),
    )
    .await;
    assert!(stale.to_string().contains("already approved"));
    assert_eq!(app.store.job(job_id).await?.gates[0].status, "approved");
    // Slack delivery failure retains a retry and doesn't roll back the decision.
    mock.fail.store(true, Ordering::SeqCst);
    due(&app).await;
    sync_one(&app).await?;
    let tries: i32 = sqlx::query_scalar("SELECT tries FROM slack_messages WHERE root_id=$1")
        .bind(job_id)
        .fetch_one(&app.store.pool)
        .await?;
    assert_eq!(tries, 1);
    mock.fail.store(false, Ordering::SeqCst);
    due(&app).await;
    sync_one(&app).await?;
    let job = finish_phase(&app, job_id).await;
    let gate = job.gates.last().unwrap().id;
    let expired = fresh_approval(&app, &mock, gate).await;
    let (_, expired_view, _) = session(&app, expired).await;
    app.store
        .mutate(job_id, |j, _| {
            j.gates.last_mut().unwrap().deadline = Utc::now() - chrono::Duration::seconds(1);
            Ok(())
        })
        .await?;
    let response = click(
        &app,
        submit(
            expired,
            &expired_view,
            "factory_approval",
            json!({"decision":choice("approve")}),
        ),
    )
    .await;
    assert!(response.to_string().contains("expired"));
    assert_eq!(
        app.store.job(job_id).await?.gates.last().unwrap().status,
        "pending"
    );
    app.store
        .mutate(job_id, |j, _| {
            j.gates.last_mut().unwrap().deadline = Utc::now() + chrono::Duration::hours(1);
            Ok(())
        })
        .await?;
    let reject = fresh_approval(&app, &mock, gate).await;
    let (_, reject_view, _) = session(&app, reject).await;
    click(
        &app,
        submit(
            reject,
            &reject_view,
            "factory_approval",
            json!({"decision":choice("reject")}),
        ),
    )
    .await;
    due(&app).await;
    sync_one(&app).await?;
    assert_eq!(app.store.job(job_id).await?.status, JobStatus::Rejected);
    let done: bool = sqlx::query_scalar("SELECT finished FROM slack_messages WHERE root_id=$1")
        .bind(job_id)
        .fetch_one(&app.store.pool)
        .await?;
    assert!(done);
    {
        let calls = mock.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|(m, v)| m == "chat.postMessage" && v["thread_ts"].is_null())
                .count(),
            1
        );
        assert!(calls.iter().any(|(m, v)| m == "chat.update"
            && v["text"].as_str().is_some_and(|s| s.contains("Rejected"))));
        assert!(!serde_json::to_string(&*calls)?.contains("test-bot-token"));
    }
    // A real engine follow-up stays on its parent's status card until the chain ends.
    let mut chain_snapshot = app.snapshot.clone();
    let workflow = chain_snapshot.workflows.get_mut("demo").unwrap();
    for p in &mut workflow.phases {
        p.gate = None;
    }
    workflow.follow_ups = vec![crate::workflow::FollowUp {
        workflow: "demo".into(),
        when: "always".into(),
        max_depth: 1,
    }];
    chain_snapshot.refresh_hash()?;
    let chain = app
        .store
        .submit(
            &Uuid::new_v4().to_string(),
            &hash("chain"),
            Submission {
                workflow: "demo".into(),
                repository: "local-demo".into(),
                issue: Issue {
                    provider: "fixture".into(),
                    key: "CHAIN-1".into(),
                    title: "Follow-up test".into(),
                    body: String::new(),
                    url: None,
                    ticket: None,
                },
            },
            app.store.job(job_id).await?.repository,
            chain_snapshot,
            "slack:U_ADMIN",
        )
        .await?;
    track(&app.store, chain.id, "C_TEST").await?;
    sync_one(&app).await?;
    for _ in 0..3 {
        finish_phase(&app, chain.id).await;
    }
    due(&app).await;
    sync_one(&app).await?;
    let finished: bool = sqlx::query_scalar("SELECT finished FROM slack_messages WHERE root_id=$1")
        .bind(chain.id)
        .fetch_one(&app.store.pool)
        .await?;
    assert!(!finished);
    let child: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE document->>'parent_id'=$1")
        .bind(chain.id.to_string())
        .fetch_one(&app.store.pool)
        .await?;
    for _ in 0..3 {
        finish_phase(&app, child).await;
    }
    due(&app).await;
    sync_one(&app).await?;
    let finished: bool = sqlx::query_scalar("SELECT finished FROM slack_messages WHERE root_id=$1")
        .bind(chain.id)
        .fetch_one(&app.store.pool)
        .await?;
    assert!(finished);
    let mut terminal_snapshot = app.snapshot.clone();
    for phase in &mut terminal_snapshot.workflows.get_mut("demo").unwrap().phases {
        phase.gate = None;
    }
    terminal_snapshot.refresh_hash()?;
    let terminal = app
        .store
        .submit(
            &Uuid::new_v4().to_string(),
            &hash("terminal-first-delivery"),
            Submission {
                workflow: "demo".into(),
                repository: "local-demo".into(),
                issue: Issue {
                    provider: "fixture".into(),
                    key: "TERMINAL-1".into(),
                    title: "Terminal delivery test".into(),
                    body: String::new(),
                    url: None,
                    ticket: None,
                },
            },
            app.store.job(job_id).await?.repository,
            terminal_snapshot,
            "slack:U_ADMIN",
        )
        .await?;
    track(&app.store, terminal.id, "C_TEST").await?;
    for _ in 0..3 {
        finish_phase(&app, terminal.id).await;
    }
    assert!(app.store.job(terminal.id).await?.status.terminal());
    let start = mock.calls.lock().unwrap().len();
    assert!(sync_one(&app).await?);
    let (milestone, finished): (Option<String>, bool) = sqlx::query_as(
        "SELECT milestone_hash,finished FROM slack_messages WHERE root_id=$1",
    )
    .bind(terminal.id)
    .fetch_one(&app.store.pool)
    .await?;
    assert!(milestone.is_some());
    assert!(finished);
    let calls = mock.calls.lock().unwrap();
    assert_eq!(
        calls[start..].iter().filter(|(m, v)| m == "chat.postMessage" && v["thread_ts"].is_null()).count(),
        1
    );
    assert_eq!(
        calls[start..].iter().filter(|(m, v)| m == "chat.postMessage" && v["thread_ts"] == "123.456").count(),
        1
    );
    drop(calls);
    sqlx::query("DELETE FROM connectors WHERE kind='slack'")
        .execute(&app.store.pool)
        .await?;
    server.abort();
    Ok(())
}
