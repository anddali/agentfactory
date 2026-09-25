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
    pub local_configuration: bool,
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
    pub async fn submit(&self, key: &str, input: Submission, actor: &str) -> Result<Job> {
        self.submit_snapshot(key, input, actor, None).await
    }
    async fn submit_snapshot(
        &self,
        key: &str,
        mut input: Submission,
        actor: &str,
        candidate: Option<Snapshot>,
    ) -> Result<Job> {
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
        let digest = if let Some(candidate) = &candidate {
            hash(serde_json::to_vec(
                &json!({"submission":input,"candidate":candidate.definition_hash}),
            )?)
        } else {
            hash(serde_json::to_vec(&input)?)
        };
        let request_key = format!("{actor}:{key}");
        if let Some(job) = self.store.existing(&request_key, &digest).await? {
            return Ok(job);
        }
        let selected = match candidate {
            Some(s) => s,
            None if self.local_configuration => {
                crate::releases::Bundle::from_snapshot(&self.snapshot, &input.workflow)?
                    .resolve(&self.platform)?
            }
            None => crate::releases::active(&self.store, &self.platform, &input.workflow).await?,
        };
        let repo = crate::repositories::resolve(
            &self.store,
            &self.executor.secret,
            &self.platform,
            &self.http,
            &input.repository,
        )
        .await?;
        ensure!(
            selected.workflows.contains_key(&input.workflow),
            "workflow is not registered"
        );
        ensure!(
            !selected.workflows[&input.workflow]
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
        let snapshot = self.executor.pin(selected, &input.workflow).await?;
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
        .route("/api/configuration", get(configuration))
        .route("/api/configuration/prompts", post(prompt_publish))
        .route("/api/configuration/prompts/revise", post(prompt_revise))
        .route("/api/configuration/prepare", post(bundle_prepare))
        .route(
            "/api/configuration/drafts/{id}/discard",
            post(draft_discard),
        )
        .route("/api/configuration/drafts/{id}", put(draft_save))
        .route("/api/configuration/validate", post(bundle_validate))
        .route("/api/configuration/releases", post(release_publish))
        .route(
            "/api/configuration/releases/{id}/export",
            get(release_export),
        )
        .route(
            "/api/configuration/releases/{id}/activate",
            post(release_activate),
        )
        .route("/api/configuration/drafts/{id}/test", post(draft_test))
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
        json!({"edit_configuration":!app.local_configuration && identity.roles.iter().any(|r|r=="configuration_editor"),"publish_configuration":!app.local_configuration && identity.roles.iter().any(|r|r=="configuration_publisher"),"subject":identity.subject,"approvable_repositories":repositories,"manage_connectors":identity.roles.iter().any(|r|r=="connector_admin"),"operable_repositories":if identity.roles.iter().any(|r|r=="operator") {identity.repositories.clone()} else {vec![]},"dynamic_approvable_repositories":if identity.roles.iter().any(|r|r=="approver") {identity.repositories.clone()} else {vec![]}}),
    ))
}
async fn catalog(State(app): State<Arc<App>>, headers: HeaderMap) -> Api<Json<Value>> {
    if !app.public_read {
        app.identity(&headers)?;
    }
    let repos: Vec<_> = app.platform.repositories.iter().filter(|(id,_)| app.read(&headers,id).is_ok()).map(|(id,r)| json!({"id":id,"provider":r.provider,"branch":r.branch,"workflow":r.workflow})).collect();
    Ok(Json(
        json!({"configuration_source":if app.local_configuration { "files" } else { "registry" },"workflows":if app.local_configuration {app.snapshot.workflows.clone()} else {crate::releases::catalog(&app.store).await?},"repositories":repos,"policy_version":app.snapshot.policy_version,"executor":app.executor.mode,"storage":if app.blobs.bucket.is_some(){"S3"}else{"filesystem"},"public_read":app.public_read}),
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
        json!({"job_id":job.id,"case_id":job.case_id,"parent_id":job.parent_id,"workflow":job.snapshot.workflows[&job.workflow],"release_id":job.snapshot.release_id,"release_digest":job.snapshot.release_digest,"definition_hash":job.snapshot.definition_hash,"repository":job.repository,"status":job.status,"created_at":job.created_at,"finished_at":job.finished_at,"policy_version":job.snapshot.policy_version,"platform_version":job.snapshot.platform_version,"platform_revision":job.snapshot.platform_revision,"worker_profiles":job.snapshot.workers,"agent_profiles":job.snapshot.agents,"prompts":prompts,"attempts":job.attempts,"gates":job.gates}),
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

fn configuration_role<'a>(
    app: &'a App,
    headers: &HeaderMap,
    publish: bool,
) -> Result<&'a Identity> {
    anyhow::ensure!(
        !app.local_configuration,
        "configuration management is disabled in local files mode"
    );
    let identity = app.identity(headers)?;
    let role = if publish {
        "configuration_publisher"
    } else {
        "configuration_editor"
    };
    anyhow::ensure!(
        identity.roles.iter().any(|r| r == role),
        "forbidden: {role} required"
    );
    Ok(identity)
}
async fn configuration(State(app): State<Arc<App>>, headers: HeaderMap) -> Api<Json<Value>> {
    let i = app.identity(&headers)?;
    ensure!(
        i.roles
            .iter()
            .any(|r| r == "configuration_editor" || r == "configuration_publisher"),
        "forbidden: configuration access required"
    );
    let mut releases: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(r) FROM workflow_releases r ORDER BY created_at DESC, id DESC",
    )
    .fetch_all(&app.store.pool)
    .await?;
    let active: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(a) FROM active_releases a ORDER BY workflow")
            .fetch_all(&app.store.pool)
            .await?;
    let mut drafts: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(d) FROM configuration_drafts d ORDER BY updated_at DESC",
    )
    .fetch_all(&app.store.pool)
    .await?;
    let revisions: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(r) FROM configuration_revisions r ORDER BY created_at DESC",
    )
    .fetch_all(&app.store.pool)
    .await?;
    let audit: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(a) FROM release_audit a ORDER BY id DESC LIMIT 200")
            .fetch_all(&app.store.pool)
            .await?;
    for entry in releases.iter_mut().chain(drafts.iter_mut()) {
        let definitions = describe_bundle(&serde_json::from_value::<crate::releases::Bundle>(
            entry["bundle"].clone(),
        )?);
        let fixture = definitions
            .values()
            .filter_map(|w| serde_json::from_value::<crate::workflow::Workflow>(w.clone()).ok())
            .all(|w| {
                std::iter::once(&w.defaults.agent_profile)
                    .chain(w.phases.iter().filter_map(|p| p.agent_profile.as_ref()))
                    .all(|id| {
                        app.platform
                            .agents
                            .get(id)
                            .is_some_and(|a| a.backend == "fixture")
                    })
            });
        entry["definitions"] = json!(definitions);
        entry["is_fixture"] = json!(fixture);
    }
    let tests: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(t) || jsonb_build_object('status',j.status,'repository',j.document->'repository'->>'id') FROM configuration_tests t JOIN jobs j ON j.id=t.job_id ORDER BY t.created_at DESC LIMIT 500").fetch_all(&app.store.pool).await?;
    let tests: Vec<_> = tests
        .into_iter()
        .filter(|t| {
            app.read(&headers, t["repository"].as_str().unwrap_or(""))
                .is_ok()
        })
        .collect();
    Ok(Json(
        json!({"releases":releases,"active":active,"drafts":drafts,"revisions":revisions,"audit":audit,"tests":tests}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftInput {
    revision: i64,
    bundle: crate::releases::Bundle,
}
async fn draft_save(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<DraftInput>,
) -> Api<Json<Value>> {
    let actor = configuration_role(&app, &headers, false)?;
    ensure!(
        serde_json::to_vec(&input.bundle)?.len() <= 2_000_000,
        "draft too large"
    );
    let revision: Option<i64> = if input.revision == 0 {
        sqlx::query_scalar("INSERT INTO configuration_drafts(id,revision,bundle,actor) VALUES($1,1,$2,$3) ON CONFLICT DO NOTHING RETURNING revision").bind(id).bind(serde_json::to_value(input.bundle)?).bind(&actor.subject).fetch_optional(&app.store.pool).await?
    } else {
        sqlx::query_scalar("UPDATE configuration_drafts SET revision=revision+1,bundle=$3,actor=$4,updated_at=now() WHERE id=$1 AND revision=$2 AND status='editing' RETURNING revision").bind(id).bind(input.revision).bind(serde_json::to_value(input.bundle)?).bind(&actor.subject).fetch_optional(&app.store.pool).await?
    };
    Ok(Json(
        json!({"id":id,"revision":revision.context("conflict: draft changed; reload before saving")?}),
    ))
}
async fn bundle_validate(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(bundle): Json<crate::releases::Bundle>,
) -> Api<Json<Value>> {
    ensure!(
        !app.local_configuration,
        "configuration management is disabled in local files mode"
    );
    let identity = app.identity(&headers)?;
    ensure!(
        identity
            .roles
            .iter()
            .any(|r| r == "configuration_editor" || r == "configuration_publisher"),
        "forbidden: configuration access required"
    );
    let s = bundle.resolve(&app.platform)?;
    Ok(Json(
        json!({"valid":true,"digest":bundle.digest()?,"workflows":s.workflows.keys().collect::<Vec<_>>(),"prompts":s.prompts.keys().collect::<Vec<_>>(),"message":"Definition validation only; no model or tools were executed."}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishInput {
    bundle: crate::releases::Bundle,
    note: String,
    #[serde(default)]
    draft_id: Option<Uuid>,
    #[serde(default)]
    draft_revision: Option<i64>,
    #[serde(default)]
    activate: bool,
}
async fn release_publish(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(input): Json<PublishInput>,
) -> Api<Json<Value>> {
    let actor = configuration_role(&app, &headers, true)?;
    ensure!(
        input.draft_id.is_some() == input.draft_revision.is_some(),
        "draft id and revision must be supplied together"
    );
    let id = crate::releases::publish_with_options(
        &app.store,
        &app.platform,
        &input.bundle,
        &actor.subject,
        &input.note,
        input.draft_id.zip(input.draft_revision),
        input.activate,
    )
    .await?;
    Ok(Json(json!({"id":id,"digest":input.bundle.digest()?})))
}
async fn release_export(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Api<Json<Value>> {
    let i = app.identity(&headers)?;
    ensure!(
        i.roles
            .iter()
            .any(|r| r == "configuration_editor" || r == "configuration_publisher"),
        "forbidden: configuration access required"
    );
    let mut bundle = crate::releases::get(&app.store, id).await?;
    bundle.base_generation =
        sqlx::query_scalar("SELECT generation FROM active_releases WHERE workflow=$1")
            .bind(&bundle.workflow)
            .fetch_optional(&app.store.pool)
            .await?
            .unwrap_or(0);
    Ok(Json(serde_json::to_value(bundle)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivateInput {
    expected_generation: i64,
    note: String,
}
async fn release_activate(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<ActivateInput>,
) -> Api<Json<Value>> {
    let actor = configuration_role(&app, &headers, true)?;
    let generation = crate::releases::activate(
        &app.store,
        &app.platform,
        id,
        input.expected_generation,
        &actor.subject,
        &input.note,
    )
    .await?;
    Ok(Json(json!({"generation":generation})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptInput {
    name: String,
    content: String,
    note: String,
}
async fn prompt_publish(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(input): Json<PromptInput>,
) -> Api<Json<Value>> {
    let actor = configuration_role(&app, &headers, true)?;
    ensure!(
        input.name.split('@').count() == 2
            && input.name.split('@').all(crate::workflow::identifier),
        "versioned prompt name required"
    );
    ensure!(
        !input.content.trim().is_empty()
            && input.content.len() <= 100_000
            && !input.note.trim().is_empty()
            && input.note.len() <= 2000,
        "prompt content and change note required"
    );
    let digest = hash(&input.content);
    let stored: String = sqlx::query_scalar("INSERT INTO configuration_revisions(kind,name,digest,content,actor,note) VALUES('prompt',$1,$2,$3,$4,$5) ON CONFLICT(kind,name) DO UPDATE SET name=EXCLUDED.name RETURNING digest").bind(&input.name).bind(&digest).bind(input.content).bind(&actor.subject).bind(input.note).fetch_one(&app.store.pool).await?;
    ensure!(
        stored == digest,
        "revision already exists with different content; increment its version"
    );
    Ok(Json(json!({"name":input.name,"digest":digest})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestInput {
    revision: i64,
    submission: Submission,
    key: String,
}
async fn draft_test(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<TestInput>,
) -> Api<Json<Value>> {
    let actor = configuration_role(&app, &headers, false)?;
    ensure!(
        actor.access(&input.submission.repository, "operator"),
        "forbidden: operator access required"
    );
    ensure!(
        app.platform
            .repositories
            .get(&input.submission.repository)
            .is_some_and(|r| r.provider == "fixture"),
        "draft tests require a designated local fixture repository"
    );
    let value: Value = sqlx::query_scalar(
        "SELECT bundle FROM configuration_drafts WHERE id=$1 AND revision=$2 AND status='editing'",
    )
    .bind(id)
    .bind(input.revision)
    .fetch_optional(&app.store.pool)
    .await?
    .context("conflict: draft changed; save and retry")?;
    let bundle: crate::releases::Bundle = serde_json::from_value(value)?;
    ensure!(
        input.submission.workflow == bundle.workflow,
        "test workflow must match draft"
    );
    let mut snapshot = bundle.resolve(&app.platform)?;
    ensure!(snapshot.agents.values().all(|a| a.backend == "fixture"), "draft tests currently require fixture agents; use local bundle execution for live harness tests");
    snapshot.release_digest = Some(bundle.digest()?);
    snapshot.refresh_hash()?;
    let job = app
        .submit_snapshot(
            &format!("draft:{id}:{}", input.key),
            input.submission,
            &actor.subject,
            Some(snapshot),
        )
        .await?;
    sqlx::query("INSERT INTO configuration_tests(job_id,draft_id,candidate_digest) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(job.id).bind(id).bind(bundle.digest()?).execute(&app.store.pool).await?;
    Ok(Json(
        json!({"job_id":job.id,"candidate_digest":bundle.digest()?}),
    ))
}

fn describe_bundle(bundle: &crate::releases::Bundle) -> std::collections::BTreeMap<String, Value> {
    bundle
        .workflows
        .iter()
        .filter_map(|(id, s)| {
            serde_yaml::from_str::<crate::workflow::Workflow>(s)
                .ok()
                .and_then(|w| serde_json::to_value(w).ok())
                .map(|w| (id.clone(), w))
        })
        .collect()
}
async fn bundle_prepare(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(bundle): Json<crate::releases::Bundle>,
) -> Api<Json<Value>> {
    configuration_role(&app, &headers, false)?;
    let bundle = crate::releases::prepare(&app.store, &app.platform, bundle).await?;
    Ok(Json(
        json!({"definitions":describe_bundle(&bundle),"digest":bundle.digest()?,"bundle":bundle}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscardInput {
    revision: i64,
}
async fn draft_discard(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<DiscardInput>,
) -> Api<Json<Value>> {
    let actor = configuration_role(&app, &headers, false)?;
    let result=sqlx::query("UPDATE configuration_drafts SET status='discarded',revision=revision+1,actor=$3,updated_at=now() WHERE id=$1 AND revision=$2 AND status='editing'").bind(id).bind(input.revision).bind(&actor.subject).execute(&app.store.pool).await?;
    ensure!(
        result.rows_affected() == 1,
        "conflict: draft changed; reload before discarding"
    );
    Ok(Json(json!({"discarded":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptConsumer {
    workflow: String,
    expected_generation: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptRevisionInput {
    name: String,
    content: String,
    note: String,
    expected_name: Option<String>,
    #[serde(default)]
    consumers: Vec<PromptConsumer>,
}
async fn prompt_revise(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(input): Json<PromptRevisionInput>,
) -> Api<Json<Value>> {
    let actor = configuration_role(&app, &headers, true)?;
    if !input.consumers.is_empty() {
        configuration_role(&app, &headers, false)?;
    }
    ensure!(
        crate::workflow::identifier(&input.name)
            && !input.content.trim().is_empty()
            && input.content.len() <= 100_000
            && !input.note.trim().is_empty()
            && input.note.len() <= 2000,
        "valid prompt name, instructions and change note required"
    );
    let mut tx = app.store.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7812451)")
        .execute(&mut *tx)
        .await?;
    let rows:Vec<(String,String)>=sqlx::query_as("SELECT name,content FROM configuration_revisions WHERE kind='prompt' ORDER BY created_at DESC,name DESC").fetch_all(&mut *tx).await?;
    let matching: Vec<_> = rows
        .iter()
        .filter(|(n, _)| n.split_once('@').is_some_and(|(s, _)| s == input.name))
        .collect();
    let latest = matching.iter().max_by_key(|(n, _)| {
        n.split_once('@')
            .and_then(|(_, v)| v.parse::<u32>().ok())
            .unwrap_or(0)
    });
    ensure!(
        latest.map(|r| &r.0) == input.expected_name.as_ref(),
        "conflict: prompt has a newer revision; reopen and review it"
    );
    let version = matching
        .iter()
        .filter_map(|(n, _)| n.split_once('@').and_then(|(_, v)| v.parse::<u32>().ok()))
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .context("prompt version exhausted")?;
    let name = if let Some((n, _)) = matching.iter().find(|(_, c)| c == &input.content) {
        n.clone()
    } else {
        format!("{}@{version}", input.name)
    };
    sqlx::query("INSERT INTO configuration_revisions(kind,name,digest,content,actor,note) VALUES('prompt',$1,$2,$3,$4,$5) ON CONFLICT DO NOTHING").bind(&name).bind(hash(&input.content)).bind(&input.content).bind(&actor.subject).bind(&input.note).execute(&mut *tx).await?;
    let mut drafts = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for consumer in input.consumers {
        ensure!(
            seen.insert(consumer.workflow.clone()),
            "duplicate workflow selection"
        );
        let row:Option<(Value,i64)>=sqlx::query_as("SELECT r.bundle,a.generation FROM active_releases a JOIN workflow_releases r ON r.id=a.release_id WHERE a.workflow=$1 FOR UPDATE OF a").bind(&consumer.workflow).fetch_optional(&mut *tx).await?;
        let (value, generation) = row.context("workflow has no active release")?;
        ensure!(
            generation == consumer.expected_generation,
            "conflict: a selected workflow changed; review its latest release"
        );
        let existing:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM configuration_drafts WHERE bundle->>'workflow'=$1 AND status='editing')").bind(&consumer.workflow).fetch_one(&mut *tx).await?;
        ensure!(
            !existing,
            "selected workflow already has a draft; adopt the prompt from that draft instead"
        );
        let mut bundle: crate::releases::Bundle = serde_json::from_value(value)?;
        bundle.base_generation = generation;
        let mut replaced = std::collections::BTreeSet::new();
        for source in bundle.workflows.values_mut() {
            let mut w: crate::workflow::Workflow = serde_yaml::from_str(source)?;
            for task in w.phases.iter_mut().flat_map(|p| &mut p.tasks) {
                if let Some(old) = task
                    .with
                    .get("prompt")
                    .cloned()
                    .filter(|n| n.split_once('@').is_some_and(|(s, _)| s == input.name))
                {
                    replaced.insert(old);
                    task.with.insert("prompt".into(), name.clone());
                }
            }
            *source = serde_yaml::to_string(&w)?;
        }
        ensure!(
            !replaced.is_empty(),
            "selected workflow does not use this prompt"
        );
        for old in replaced {
            bundle.prompts.remove(&old);
        }
        bundle.prompts.insert(name.clone(), input.content.clone());
        bundle.resolve(&app.platform)?;
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO configuration_drafts(id,revision,bundle,actor) VALUES($1,1,$2,$3)",
        )
        .bind(id)
        .bind(serde_json::to_value(&bundle)?)
        .bind(&actor.subject)
        .execute(&mut *tx)
        .await?;
        drafts.push(json!({"id":id,"workflow":bundle.workflow}));
    }
    tx.commit().await?;
    Ok(Json(json!({"name":name,"drafts":drafts})))
}
