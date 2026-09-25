use crate::{
    config::{Platform, Snapshot},
    execution::{secure_equal, token, verify_hmac, Executor},
    model::*,
    storage::Blobs,
    store::Store,
};
use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tower_http::services::ServeDir;
use uuid::Uuid;
macro_rules! ensure { ($condition:expr, $($message:tt)*) => { if !$condition { return Err(anyhow::anyhow!($($message)*).into()); } }; }

#[derive(Clone)]
pub struct App {
    pub store: Store,
    pub platform: Platform,
    pub snapshot: Snapshot,
    pub executor: Executor,
    pub blobs: Blobs,
    pub http: reqwest::Client,
    pub identities: Vec<Identity>,
    pub public_read: bool,
}
#[derive(Clone, Deserialize)]
pub struct Identity {
    pub token: String,
    pub subject: String,
    pub roles: Vec<String>,
    pub repositories: Vec<String>,
}
impl Identity {
    fn access(&self, repo: &str, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
            && self.repositories.iter().any(|r| r == repo || r == "*")
    }
}
pub struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = self.0.to_string();
        let status = if message.starts_with("unauthorized") {
            StatusCode::UNAUTHORIZED
        } else if message.starts_with("forbidden") {
            StatusCode::FORBIDDEN
        } else if self
            .0
            .downcast_ref::<sqlx::Error>()
            .is_some_and(|e| matches!(e, sqlx::Error::RowNotFound))
        {
            StatusCode::NOT_FOUND
        } else if self.0.downcast_ref::<sqlx::Error>().is_some() {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::CONFLICT
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error=%self.0, "API failure");
        }
        (status, Json(json!({"error":if status == StatusCode::INTERNAL_SERVER_ERROR { "Database operation failed" } else { &message }}))).into_response()
    }
}
type Api<T> = std::result::Result<T, ApiError>;
fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
}
impl App {
    fn identity(&self, headers: &HeaderMap) -> Result<&Identity> {
        self.identities
            .iter()
            .find(|i| secure_equal(&i.token, bearer(headers)))
            .context("unauthorized: valid API credential required")
    }
    fn approver(&self, identity: &Identity, repo: &str) -> bool {
        identity.access(repo, "approver")
            && self
                .platform
                .repositories
                .get(repo)
                .is_none_or(|r| r.maintainers.contains(&identity.subject))
    }
    fn read(&self, headers: &HeaderMap, repo: &str) -> Result<()> {
        if self.public_read {
            return Ok(());
        }
        ensure!(
            self.identity(headers)?.access(repo, "observer"),
            "forbidden: repository observation access required"
        );
        Ok(())
    }
    async fn worker_job(&self, headers: &HeaderMap, attempt: Uuid) -> Result<Job> {
        ensure!(
            secure_equal(bearer(headers), &token(&self.executor.secret, attempt)),
            "unauthorized: invalid attempt token"
        );
        self.store.attempt_job(attempt).await
    }
    pub async fn submit(&self, key: &str, mut input: Submission, actor: &str) -> Result<Job> {
        ensure!(
            !key.is_empty() && key.len() <= 200,
            "Idempotency-Key must contain 1–200 characters"
        );
        ensure!(
            !input.issue.key.is_empty()
                && input.issue.key.len() <= 120
                && input.issue.title.len() <= 500
                && input.issue.body.len() <= 100_000,
            "invalid issue metadata"
        );
        ensure!(
            matches!(
                input.issue.provider.as_str(),
                "jira" | "github" | "github_pr" | "ado_pr" | "manual" | "fixture"
            ),
            "unsupported issue provider"
        );
        let digest = hash(serde_json::to_vec(&input)?);
        let request_key = format!("{actor}:{key}");
        if let Some(job) = self.store.existing(&request_key, &digest).await? {
            return Ok(job);
        }
        let repo = crate::repositories::resolve(
            &self.store,
            &self.executor.secret,
            &self.platform,
            &self.http,
            &input.repository,
        )
        .await?;
        ensure!(
            self.snapshot.workflows.contains_key(&input.workflow),
            "workflow is not registered"
        );
        ensure!(
            !self.snapshot.workflows[&input.workflow]
                .phases
                .iter()
                .flat_map(|p| p.inputs.values())
                .any(|v| v.starts_with("parent.")),
            "this workflow requires a parent job and cannot be launched directly"
        );
        if input.issue.provider == "jira" {
            if let Some(c) =
                crate::connectors::load(&self.store, &self.executor.secret, "jira").await?
            {
                c.authorize(&input.repository)?;
                let response = crate::jira::issue(&c, &input.issue.key).await?;
                input.issue.title = response["summary"]
                    .as_str()
                    .context("Jira issue summary missing")?
                    .into();
                input.issue.body = response["description"].as_str().unwrap_or("").into();
                input.issue.url = Some(format!(
                    "{}/browse/{}",
                    c.value("base_url")?.trim_end_matches('/'),
                    input.issue.key
                ));
                ensure!(
                    input.issue.body.len() <= 100_000 && input.issue.title.len() <= 500,
                    "Jira issue exceeds size limit"
                );
            }
        }
        let mut work_branch = String::new();
        let mut base_branch = repo.branch.clone();
        let revision = if matches!(input.issue.provider.as_str(), "github_pr" | "ado_pr") {
            ensure!(
                (input.issue.provider == "github_pr" && repo.provider == "github")
                    || (input.issue.provider == "ado_pr" && repo.provider == "ado"),
                "PR provider does not match repository"
            );
            let number: u64 = input
                .issue
                .key
                .parse()
                .context("PR key must be its numeric id")?;
            let path = if repo.provider == "github" {
                format!("{}/pulls/{number}", repo.api_url.trim_end_matches('/'))
            } else {
                format!(
                    "{}/pullrequests/{number}?api-version=7.1",
                    repo.api_url.trim_end_matches('/')
                )
            };
            let mut request = self.http.get(path);
            if let Some(credential) = crate::connectors::repository_token(
                &self.store,
                &self.executor.secret,
                &input.repository,
                &repo,
                false,
            )
            .await?
            {
                request = if repo.provider == "ado" {
                    request.basic_auth("", Some(credential))
                } else {
                    request.bearer_auth(credential)
                };
            }
            let response: Value = request.send().await?.error_for_status()?.json().await?;
            let context =
                crate::providers::pull_request_context(&repo.provider, &repo.url, &response)?;
            work_branch = context.1;
            base_branch = context.2;
            context.0
        } else if let Some(sha) = &repo.revision {
            sha.clone()
        } else {
            let mut url = reqwest::Url::parse(&repo.api_url)?;
            match repo.provider.as_str() {
                "github" => {
                    url.path_segments_mut()
                        .map_err(|_| anyhow::anyhow!("invalid repository API URL"))?
                        .extend(["commits", &repo.branch]);
                }
                "ado" => {
                    url.path_segments_mut()
                        .map_err(|_| anyhow::anyhow!("invalid repository API URL"))?
                        .push("refs");
                    url.query_pairs_mut()
                        .append_pair("filter", &format!("heads/{}", repo.branch))
                        .append_pair("api-version", "7.1");
                }
                _ => anyhow::bail!("fixture repository needs a pinned revision"),
            }
            let mut request = self.http.get(url);
            if let Some(credential) = crate::connectors::repository_token(
                &self.store,
                &self.executor.secret,
                &input.repository,
                &repo,
                false,
            )
            .await?
            {
                request = if repo.provider == "ado" {
                    request.basic_auth("", Some(credential))
                } else {
                    request.bearer_auth(credential)
                };
            }
            let response: Value = request.send().await?.error_for_status()?.json().await?;
            if repo.provider == "ado" {
                response["value"]
                    .as_array()
                    .and_then(|refs| {
                        refs.iter().find(|r| {
                            r["name"].as_str() == Some(&format!("refs/heads/{}", repo.branch))
                        })
                    })
                    .and_then(|r| r["objectId"].as_str())
            } else {
                response["sha"].as_str()
            }
            .context("provider did not return a repository revision")?
            .into()
        };
        ensure!(
            revision.len() == 40 && revision.bytes().all(|c| c.is_ascii_hexdigit()),
            "repository revision must be a full commit SHA"
        );
        let snapshot = self
            .executor
            .pin(self.snapshot.clone(), &input.workflow)
            .await?;
        let repository = Repository {
            id: input.repository.clone(),
            url: repo.url.clone(),
            revision,
            base_branch,
            work_branch,
            provider: repo.provider.clone(),
            api_url: repo.api_url.clone(),
        };
        self.store
            .submit(&request_key, &digest, input, repository, snapshot, actor)
            .await
    }
}
pub fn router(app: Arc<App>, web: &str) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/access", get(access))
        .route("/api/connectors", get(connectors_list))
        .route("/api/connectors/{kind}", put(connectors_save))
        .route("/api/connectors/{kind}/test", post(connectors_test))
        .route("/api/catalog", get(catalog))
        .route("/api/jira/issues", get(jira_search))
        .route("/api/connectors/jira/fields", post(jira_fields))
        .route("/api/connectors/jira/preview/{key}", post(jira_preview))
        .route("/api/jira/issues/{key}", get(jira_details))
        .route("/api/jobs", get(jobs).post(submit))
        .route("/api/jobs/{id}", get(job))
        .route("/api/jobs/{id}/receipt", get(receipt))
        .route("/api/jobs/{id}/cancel", post(cancel))
        .route("/api/jobs/{id}/attention", put(attention))
        .route("/api/jobs/{id}/decisions", post(decide))
        .route("/api/events", get(events))
        .route("/api/artifacts/{id}", get(artifact))
        .route("/worker/{attempt}/claim", post(claim))
        .route("/worker/{attempt}/heartbeat", post(heartbeat))
        .route("/worker/{attempt}/complete", post(complete))
        .route("/worker/{attempt}/artifacts/{name}", put(upload))
        .route("/worker/{attempt}/inputs/{name}", get(input))
        .route("/hooks/github", post(github))
        .route("/hooks/jira", post(jira))
        .route("/hooks/slack", post(slack))
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-store"),
                );
                response
            },
        ))
        .fallback_service(ServeDir::new(web).append_index_html_on_directories(true))
        .with_state(app)
}
async fn health(State(app): State<Arc<App>>) -> Api<Json<Value>> {
    sqlx::query("SELECT 1").execute(&app.store.pool).await?;
    Ok(Json(
        json!({"status":"ok","version":env!("CARGO_PKG_VERSION"),"executor":app.executor.mode}),
    ))
}
async fn access(State(app): State<Arc<App>>, headers: HeaderMap) -> Api<Json<Value>> {
    if bearer(&headers).is_empty() && app.public_read {
        return Ok(Json(
            json!({"subject":null,"approvable_repositories":[],"manage_connectors":false,"operable_repositories":[],"dynamic_approvable_repositories":[]}),
        ));
    }
    let identity = app.identity(&headers)?;
    let repositories: Vec<_> = app
        .platform
        .repositories
        .iter()
        .filter(|(id, repo)| {
            identity.access(id, "approver") && repo.maintainers.contains(&identity.subject)
        })
        .map(|(id, _)| id)
        .collect();
    Ok(Json(
        json!({"subject":identity.subject,"approvable_repositories":repositories,"manage_connectors":identity.roles.iter().any(|r|r=="connector_admin"),"operable_repositories":if identity.roles.iter().any(|r|r=="operator") {identity.repositories.clone()} else {vec![]},"dynamic_approvable_repositories":if identity.roles.iter().any(|r|r=="approver") {identity.repositories.clone()} else {vec![]}}),
    ))
}
async fn catalog(State(app): State<Arc<App>>, headers: HeaderMap) -> Api<Json<Value>> {
    if !app.public_read {
        app.identity(&headers)?;
    }
    let repos: Vec<_> = app.platform.repositories.iter().filter(|(id,_)| app.read(&headers,id).is_ok()).map(|(id,r)| json!({"id":id,"provider":r.provider,"branch":r.branch,"workflow":r.workflow})).collect();
    Ok(Json(
        json!({"workflows":app.snapshot.workflows,"repositories":repos,"policy_version":app.snapshot.policy_version,"executor":app.executor.mode,"storage":if app.blobs.bucket.is_some(){"S3"}else{"filesystem"},"public_read":app.public_read}),
    ))
}
fn summary(j: &Job) -> Value {
    json!({"id":j.id,"case_id":j.case_id,"parent_id":j.parent_id,"workflow":j.workflow,"repository":j.repository.id,"issue":j.issue,"status":j.status,"attention_dismissed":j.attention_dismissed,"phase":j.phase().id,"attempts":j.attempts.len(),"gates":j.gates,"created_at":j.created_at,"finished_at":j.finished_at,"agent_ms":j.attempts.iter().filter_map(|a|a.result.as_ref()).flat_map(|r|&r.tasks).filter(|t|t.task=="agent.execute").map(|t|t.duration_ms).sum::<u64>()})
}
async fn jobs(State(app): State<Arc<App>>, headers: HeaderMap) -> Api<Json<Value>> {
    if !app.public_read {
        app.identity(&headers)?;
    }
    let jobs = app.store.jobs().await?;
    Ok(Json(
        json!({"jobs":jobs.iter().filter(|j|app.read(&headers,&j.repository.id).is_ok()).map(summary).collect::<Vec<_>>(),"limit":500}),
    ))
}
async fn job(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Api<Json<Value>> {
    let job = app.store.job(id).await?;
    app.read(&headers, &job.repository.id)?;
    let events = app.store.events(Some(id)).await?;
    Ok(Json(json!({"job":job,"events":events})))
}
async fn receipt(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Api<Json<Value>> {
    let job = app.store.job(id).await?;
    app.read(&headers, &job.repository.id)?;
    let prompts: Value = job
        .snapshot
        .prompts
        .iter()
        .map(|(k, v)| (k.clone(), json!({"sha256":v.sha256})))
        .collect();
    Ok(Json(
        json!({"job_id":job.id,"case_id":job.case_id,"parent_id":job.parent_id,"workflow":job.snapshot.workflows[&job.workflow],"definition_hash":job.snapshot.definition_hash,"repository":job.repository,"status":job.status,"created_at":job.created_at,"finished_at":job.finished_at,"policy_version":job.snapshot.policy_version,"platform_version":job.snapshot.platform_version,"platform_revision":job.snapshot.platform_revision,"worker_profiles":job.snapshot.workers,"agent_profiles":job.snapshot.agents,"prompts":prompts,"attempts":job.attempts,"gates":job.gates}),
    ))
}
async fn events(State(app): State<Arc<App>>, headers: HeaderMap) -> Api<Json<Value>> {
    if !app.public_read {
        app.identity(&headers)?;
    }
    let jobs = app.store.jobs().await?;
    let allowed: std::collections::HashSet<_> = jobs
        .iter()
        .filter(|j| app.read(&headers, &j.repository.id).is_ok())
        .map(|j| j.id)
        .collect();
    Ok(Json(
        json!({"events":app.store.events(None).await?.into_iter().filter(|e|allowed.contains(&e.job_id)).collect::<Vec<_>>()}),
    ))
}
#[derive(Deserialize)]
struct JiraSearch {
    query: String,
}
async fn jira_fields(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(input): Json<crate::connectors::Update>,
) -> Api<Json<Value>> {
    ensure!(
        app.identity(&headers)?
            .roles
            .iter()
            .any(|r| r == "connector_admin"),
        "forbidden: connector administrator required"
    );
    let c = crate::connectors::jira_draft(&app.store, &app.executor.secret, &app.platform, input)
        .await?;
    let url = reqwest::Url::parse(&format!(
        "{}/rest/api/3/field",
        c.jira_api_base()?.trim_end_matches('/')
    ))?;
    let data = crate::jira::get(&c, url).await?;
    let fields: Vec<_> = data
        .as_array()
        .context("Invalid Jira field catalog")?
        .iter()
        .filter(|f| f["id"] != "summary" && f["id"] != "description")
        .map(|f| json!({"id":f["id"],"name":f["name"]}))
        .collect();
    Ok(Json(json!({"fields":fields})))
}
async fn jira_preview(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Json(input): Json<crate::connectors::Update>,
) -> Api<Json<Value>> {
    ensure!(
        app.identity(&headers)?
            .roles
            .iter()
            .any(|r| r == "connector_admin"),
        "forbidden: connector administrator required"
    );
    let c = crate::connectors::jira_draft(&app.store, &app.executor.secret, &app.platform, input)
        .await?;
    Ok(Json(crate::jira::issue(&c, &key).await?))
}
async fn jira_details(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Api<Json<Value>> {
    let identity = app.identity(&headers)?;
    ensure!(
        identity.roles.iter().any(|r| r == "operator") && !identity.repositories.is_empty(),
        "forbidden: operator access required"
    );
    ensure!(
        !key.is_empty() && key.len() <= 120,
        "Invalid Jira issue key"
    );
    let c = crate::connectors::load(&app.store, &app.executor.secret, "jira")
        .await?
        .context("Configure the Jira connector before loading issues")?;
    c.authorize("")?;
    Ok(Json(crate::jira::issue(&c, &key).await?))
}

async fn jira_search(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(input): Query<JiraSearch>,
) -> Api<Json<Value>> {
    let identity = app.identity(&headers)?;
    ensure!(
        identity.roles.iter().any(|r| r == "operator") && !identity.repositories.is_empty(),
        "forbidden: operator access required"
    );
    let query = input.query.trim();
    ensure!(
        (2..=120).contains(&query.chars().count()),
        "Enter between 2 and 120 characters"
    );
    let c = crate::connectors::load(&app.store, &app.executor.secret, "jira")
        .await?
        .context("Configure the Jira connector before searching")?;
    c.authorize("")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .get(format!(
            "{}/rest/api/3/issue/picker",
            c.jira_api_base()?.trim_end_matches('/')
        ))
        .basic_auth(c.value("email")?, Some(c.value("token")?))
        .query(&[
            ("query", query),
            ("currentJQL", "order by updated DESC"),
            ("showSubTasks", "true"),
        ])
        .send()
        .await
        .context("Jira search request failed")?;
    ensure!(
        response.status().is_success(),
        "Jira search failed; check connector credentials, scopes and Jira access"
    );
    let data: Value = response
        .json()
        .await
        .context("Invalid Jira search response")?;
    let mut issues = Vec::new();
    let mut seen = std::collections::HashSet::new();
    if let Some(sections) = data["sections"].as_array() {
        for section in sections {
            if let Some(items) = section["issues"].as_array() {
                for item in items {
                    if let (Some(key), Some(summary)) =
                        (item["key"].as_str(), item["summaryText"].as_str())
                    {
                        if issues.len() < 20 && seen.insert(key.to_owned()) {
                            issues.push(json!({"key":key,"summary":summary}));
                        }
                    }
                }
            }
        }
    }
    Ok(Json(json!({"issues":issues})))
}

async fn submit(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(input): Json<Submission>,
) -> Api<Json<Value>> {
    let identity = app.identity(&headers)?;
    ensure!(
        identity.access(&input.repository, "operator"),
        "forbidden: repository operator access required"
    );
    let key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .context("Idempotency-Key required")?;
    Ok(Json(summary(
        &app.submit(key, input, &identity.subject).await?,
    )))
}
async fn cancel(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Api<Json<Value>> {
    let identity = app.identity(&headers)?;
    let job = app.store.job(id).await?;
    ensure!(
        identity.access(&job.repository.id, "operator"),
        "forbidden: operator access required"
    );
    Ok(Json(summary(
        &app.store
            .mutate(id, |j, c| j.cancel(&identity.subject, Utc::now(), c))
            .await?,
    )))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttentionUpdate {
    dismissed: bool,
}
async fn attention(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<AttentionUpdate>,
) -> Api<Json<Value>> {
    let identity = app.identity(&headers)?;
    let job = app.store.job(id).await?;
    ensure!(
        identity.access(&job.repository.id, "operator"),
        "forbidden: operator access required"
    );
    Ok(Json(summary(
        &app.store
            .mutate(id, |j, c| {
                j.set_attention_dismissed(input.dismissed, &identity.subject, Utc::now(), c)
            })
            .await?,
    )))
}
async fn decide(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<Decision>,
) -> Api<Json<Value>> {
    let identity = app.identity(&headers)?;
    let job = app.store.job(id).await?;
    ensure!(
        app.approver(identity, &job.repository.id),
        "forbidden: repository maintainer required"
    );
    ensure!(
        !input.event_id.is_empty() && input.event_id.len() <= 200,
        "decision event id required"
    );
    Ok(Json(summary(
        &app.store
            .mutate(id, |j, c| {
                j.decide(&input, &identity.subject, "api", Utc::now(), c)
            })
            .await?,
    )))
}
#[derive(Serialize, Deserialize)]
pub struct Claim {
    pub instance: Uuid,
}
fn instance(headers: &HeaderMap) -> Result<Uuid> {
    Ok(headers
        .get("X-Worker-Instance")
        .and_then(|h| h.to_str().ok())
        .context("worker instance required")?
        .parse()?)
}
fn manifest(job: &Job) -> Result<Manifest> {
    let mut inputs = std::collections::BTreeMap::new();
    for (name, reference) in &job.phase().inputs {
        let parts: Vec<_> = reference.split('.').collect();
        let artifact = if parts[0] == "parent" {
            job.upstream_artifacts.get(parts[2])
        } else {
            job.attempts
                .iter()
                .rev()
                .find(|a| a.phase == parts[1] && a.status == "succeeded")
                .and_then(|a| a.artifacts.get(parts[3]))
        }
        .context("declared input artifact unavailable")?;
        inputs.insert(name.clone(), artifact.clone());
    }
    Ok(Manifest {
        job_id: job.id,
        attempt_id: job.current().id,
        phase: job.phase().clone(),
        repository: job.repository.clone(),
        issue: job.issue.clone(),
        prompts: job.snapshot.prompts.clone(),
        agent: job.snapshot.agents[job
            .phase()
            .agent_profile
            .as_ref()
            .unwrap_or(&job.snapshot.workflows[&job.workflow].defaults.agent_profile)]
        .clone(),
        validations: job.snapshot.validations.clone(),
        inputs,
        deadline: job.current().deadline,
        commit_time: job.created_at,
        credentials: WorkerCredentials::default(),
    })
}
async fn claim(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(attempt): Path<Uuid>,
    Json(input): Json<Claim>,
) -> Api<Json<Manifest>> {
    let job = app.worker_job(&headers, attempt).await?;
    let job = app
        .store
        .mutate(job.id, |j, c| {
            j.claim(attempt, input.instance, Utc::now(), c)
        })
        .await?;
    let mut manifest = manifest(&job)?;
    let permissions = &job.phase().permissions;
    let write = permissions
        .iter()
        .any(|p| p == "repository.branch.write" || p == "pull_request.create");
    if write || permissions.iter().any(|p| p == "repository.read") {
        // Check the pinned destination too before issuing a credential to a worker.
        let pinned = crate::repositories::pinned(&app.platform, &job.repository);
        manifest.credentials.repository_token = crate::connectors::repository_token(
            &app.store,
            &app.executor.secret,
            &job.repository.id,
            &pinned,
            write,
        )
        .await?;
    }
    if job.phase().tasks.iter().any(|t| t.uses == "agent.execute") {
        for key in &manifest.agent.env_keys {
            manifest.credentials.agent_env.insert(
                key.clone(),
                std::env::var(key)
                    .with_context(|| format!("agent credential {key} is not configured"))?,
            );
        }
    }
    Ok(Json(manifest))
}
async fn heartbeat(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(attempt): Path<Uuid>,
) -> Api<Json<Value>> {
    let job = app.worker_job(&headers, attempt).await?;
    let instance = instance(&headers)?;
    app.store
        .mutate(job.id, |j, _| {
            j.worker(attempt, instance, Utc::now())?;
            j.current_mut().heartbeat_at = Some(Utc::now());
            Ok(())
        })
        .await?;
    Ok(Json(json!({"ok":true})))
}
async fn complete(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(attempt): Path<Uuid>,
    Json(result): Json<Completion>,
) -> Api<Json<Value>> {
    let job = app.worker_job(&headers, attempt).await?;
    let instance = instance(&headers)?;
    let job = app
        .store
        .mutate(job.id, |j, c| {
            j.complete(attempt, instance, result, Utc::now(), c)
        })
        .await?;
    Ok(Json(summary(&job)))
}
async fn upload(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path((attempt, name)): Path<(Uuid, String)>,
    body: Bytes,
) -> Api<Json<Artifact>> {
    let job = app.worker_job(&headers, attempt).await?;
    let instance = instance(&headers)?;
    job.worker(attempt, instance, Utc::now())?;
    ensure!(
        job.phase()
            .tasks
            .iter()
            .any(|t| t.uses == "artifact.publish" && t.with.get("name") == Some(&name)),
        "artifact not declared by phase"
    );
    let sha256 = app.blobs.put(&body).await?;
    let artifact = Artifact {
        id: Uuid::new_v4(),
        attempt_id: attempt,
        name: name.clone(),
        sha256,
        size: body.len(),
        created_at: Utc::now(),
    };
    let job = app
        .store
        .mutate(job.id, |j, _| {
            j.worker(attempt, instance, Utc::now())?;
            if let Some(old) = j.current().artifacts.get(&name) {
                ensure!(
                    old.sha256 == artifact.sha256,
                    "artifact names are immutable within an attempt"
                );
            } else {
                j.current_mut().artifacts.insert(name.clone(), artifact);
            }
            Ok(())
        })
        .await?;
    Ok(Json(job.current().artifacts[&name].clone()))
}
async fn input(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path((attempt, name)): Path<(Uuid, String)>,
) -> Api<impl IntoResponse> {
    let job = app.worker_job(&headers, attempt).await?;
    job.worker(attempt, instance(&headers)?, Utc::now())?;
    let manifest = manifest(&job)?;
    let artifact = manifest
        .inputs
        .get(&name)
        .context("input not declared for this phase")?;
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        app.blobs.get(&artifact.sha256).await?,
    ))
}
async fn artifact(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Api<impl IntoResponse> {
    let row: Value = sqlx::query_scalar("SELECT document FROM jobs WHERE jsonb_path_exists(document, '$.attempts[*].artifacts.* ? (@.id == $id)', jsonb_build_object('id',$1::text)) LIMIT 1").bind(id.to_string()).fetch_one(&app.store.pool).await?;
    let job: Job = serde_json::from_value(row)?;
    app.read(&headers, &job.repository.id)?;
    let artifact = job
        .attempts
        .iter()
        .flat_map(|a| a.artifacts.values())
        .find(|a| a.id == id)
        .context("artifact not found")?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment"),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff",
            ),
        ],
        app.blobs.get(&artifact.sha256).await?,
    ))
}

async fn github(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Api<Json<Value>> {
    let secret = crate::connectors::credential(
        &app.store,
        &app.executor.secret,
        "github",
        "webhook_secret",
        "FACTORY_GITHUB_WEBHOOK_SECRET",
    )
    .await?;
    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("sha256="))
        .unwrap_or("");
    ensure!(
        verify_hmac(&secret, &body, signature),
        "unauthorized: invalid GitHub signature"
    );
    let payload: Value = serde_json::from_slice(&body)?;
    let event = headers
        .get("X-GitHub-Event")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if event != "issues"
        || payload["action"] != "labeled"
        || payload["label"]["name"] != "agent-ready"
    {
        return Ok(Json(json!({"ignored":true})));
    }
    let url = payload["repository"]["clone_url"]
        .as_str()
        .context("repository clone URL missing")?;
    let alias = app
        .platform
        .repositories
        .iter()
        .find(|(_, r)| r.provider == "github" && r.url == url);
    let (repository, workflow) = alias
        .map(|(id, r)| (id.clone(), r.workflow.clone()))
        .unwrap_or_else(|| (url.into(), app.platform.default_workflow.clone()));
    let number = payload["issue"]["number"]
        .as_u64()
        .context("issue number missing")?;
    let request = Submission {
        workflow,
        repository,
        issue: Issue {
            provider: "github".into(),
            key: number.to_string(),
            title: payload["issue"]["title"].as_str().unwrap_or("").into(),
            body: payload["issue"]["body"].as_str().unwrap_or("").into(),
            url: payload["issue"]["html_url"].as_str().map(str::to_owned),
        },
    };
    let delivery = headers
        .get("X-GitHub-Delivery")
        .and_then(|h| h.to_str().ok())
        .context("delivery id missing")?;
    Ok(Json(summary(
        &app.submit(delivery, request, "github-webhook").await?,
    )))
}
#[derive(Deserialize)]
struct JiraEvent {
    #[serde(default)]
    workflow: Option<String>,
    event_id: String,
    repository: String,
    issue: JiraIssue,
}
#[derive(Deserialize)]
struct JiraIssue {
    key: String,
    fields: JiraFields,
}
#[derive(Deserialize)]
struct JiraFields {
    summary: String,
    description: Option<Value>,
    labels: Vec<String>,
}
async fn jira(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(payload): Json<JiraEvent>,
) -> Api<Json<Value>> {
    let secret = crate::connectors::credential(
        &app.store,
        &app.executor.secret,
        "jira",
        "webhook_secret",
        "FACTORY_JIRA_WEBHOOK_SECRET",
    )
    .await?;
    ensure!(
        secure_equal(bearer(&headers), &secret),
        "unauthorized: invalid Jira credential"
    );
    if !payload
        .issue
        .fields
        .labels
        .iter()
        .any(|l| l == "agent-ready")
    {
        return Ok(Json(json!({"ignored":true})));
    }
    let workflow = payload.workflow.unwrap_or_else(|| {
        app.platform
            .repositories
            .get(&payload.repository)
            .map(|r| r.workflow.clone())
            .unwrap_or_else(|| app.platform.default_workflow.clone())
    });
    let request = Submission {
        workflow,
        repository: payload.repository,
        issue: Issue {
            provider: "jira".into(),
            key: payload.issue.key,
            title: payload.issue.fields.summary,
            body: payload
                .issue
                .fields
                .description
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_default(),
            url: None,
        },
    };
    Ok(Json(summary(
        &app.submit(&payload.event_id, request, "jira-webhook")
            .await?,
    )))
}
async fn slack(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Api<Json<Value>> {
    let secret = crate::connectors::credential(
        &app.store,
        &app.executor.secret,
        "slack",
        "signing_secret",
        "FACTORY_SLACK_SIGNING_SECRET",
    )
    .await?;
    let stamp = headers
        .get("X-Slack-Request-Timestamp")
        .and_then(|h| h.to_str().ok())
        .context("unauthorized: missing Slack timestamp")?;
    let timestamp: i64 = stamp.parse()?;
    ensure!(
        (Utc::now().timestamp() - timestamp).abs() <= 300,
        "unauthorized: expired Slack signature"
    );
    let signature = headers
        .get("X-Slack-Signature")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("v0="))
        .unwrap_or("");
    let mut signed = format!("v0:{stamp}:").into_bytes();
    signed.extend(&body);
    ensure!(
        verify_hmac(&secret, &signed, signature),
        "unauthorized: invalid Slack signature"
    );
    let form: std::collections::BTreeMap<_, _> = reqwest::Url::parse(&format!(
        "https://callback.invalid/?{}",
        std::str::from_utf8(&body)?
    ))?
    .query_pairs()
    .map(|(k, v)| (k.into_owned(), v.into_owned()))
    .collect();
    let text = form.get("text").context("command text missing")?;
    let args: Vec<_> = text.split_whitespace().collect();
    ensure!(
        args.len() == 3 && matches!(args[0], "approve" | "reject"),
        "Usage: /factory approve|reject <gate-id> <artifact-digest>"
    );
    let gate: Uuid = args[1].parse()?;
    let row: Value =
        sqlx::query_scalar("SELECT document FROM jobs WHERE document->'gates' @> $1::jsonb")
            .bind(json!([{"id":gate}]))
            .fetch_one(&app.store.pool)
            .await?;
    let job: Job = serde_json::from_value(row)?;
    let channel = crate::connectors::slack_channel(
        &app.store,
        &app.executor.secret,
        &app.platform,
        &job.repository.id,
    )
    .await?;
    let subject = format!("slack:{}", form.get("user_id").context("user missing")?);
    let authorized = app
        .platform
        .repositories
        .get(&job.repository.id)
        .map_or_else(
            || {
                app.identities
                    .iter()
                    .any(|i| i.subject == subject && i.access(&job.repository.id, "approver"))
            },
            |r| r.maintainers.contains(&subject),
        );
    ensure!(
        authorized && Some(&channel) == form.get("channel_id"),
        "forbidden: approval must come from a maintainer in the configured channel"
    );
    let decision = Decision {
        event_id: hash(&body),
        gate_id: gate,
        artifact_digest: args[2].into(),
        approve: args[0] == "approve",
    };
    app.store
        .mutate(job.id, |j, c| {
            j.decide(&decision, &subject, "slack", Utc::now(), c)
        })
        .await?;
    Ok(Json(
        json!({"response_type":"ephemeral","text":"Decision recorded against the exact phase attempt and artifact version."}),
    ))
}

async fn connectors_list(State(app): State<Arc<App>>, headers: HeaderMap) -> Api<Json<Value>> {
    ensure!(
        app.identity(&headers)?
            .roles
            .iter()
            .any(|r| r == "connector_admin"),
        "forbidden: connector administrator required"
    );
    Ok(Json(
        crate::connectors::list(&app.store, &app.executor.secret).await?,
    ))
}
async fn connectors_save(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(kind): Path<String>,
    Json(input): Json<crate::connectors::Update>,
) -> Api<Json<Value>> {
    let identity = app.identity(&headers)?;
    ensure!(
        identity.roles.iter().any(|r| r == "connector_admin"),
        "forbidden: connector administrator required"
    );
    crate::connectors::save(
        &app.store,
        &app.executor.secret,
        &app.platform,
        &kind,
        input,
        &identity.subject,
    )
    .await?;
    Ok(Json(json!({"ok":true})))
}

async fn connectors_test(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(kind): Path<String>,
    Json(input): Json<crate::connectors::Update>,
) -> Api<Json<Value>> {
    ensure!(
        app.identity(&headers)?
            .roles
            .iter()
            .any(|r| r == "connector_admin"),
        "forbidden: connector administrator required"
    );
    Ok(Json(
        crate::connectors::test_connection(
            &app.store,
            &app.executor.secret,
            &app.platform,
            &kind,
            input,
        )
        .await?,
    ))
}
