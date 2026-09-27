//! Slack is a UI for existing jobs, never an alternative execution or approval engine.
use crate::{
    api::{App, Identity},
    execution::verify_hmac,
    model::*,
    workflow::Workflow,
};
use anyhow::{bail, ensure, Context, Result};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;

#[cfg(test)]
static TEST_SLACK_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
#[cfg(test)]
mod integration_tests;

#[derive(Debug)]
struct SlackBackoff(u32);
impl std::fmt::Display for SlackBackoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Slack rate limit; retry after {} seconds", self.0)
    }
}
impl std::error::Error for SlackBackoff {}

#[derive(sqlx::FromRow)]
struct MessageRow {
    root_id: Uuid,
    channel_id: String,
    message_ts: Option<String>,
    rendered_hash: Option<String>,
    milestone_hash: Option<String>,
}

fn plain(s: &str) -> Value {
    json!({"type":"plain_text","text":s,"emoji":false})
}
fn section(s: &str) -> Value {
    json!({"type":"section","text":plain(&clip(s, 2900))})
}
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.into()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
fn slack_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn button(label: &str, action: &str, value: &str) -> Value {
    json!({"type":"button","text":plain(label),"action_id":action,"value":value})
}
fn modal(id: Uuid, callback: &str, title: &str, blocks: Vec<Value>, submit: Option<&str>) -> Value {
    let mut v = json!({"type":"modal","callback_id":callback,"private_metadata":id.to_string(),"title":plain(title),"close":plain("Close"),"blocks":blocks});
    if let Some(s) = submit {
        v["submit"] = plain(s);
    }
    v
}
fn progress(id: Uuid, text: &str) -> Value {
    modal(id, "factory_wait", "Factory", vec![section(text)], None)
}
fn update(view: Value) -> Value {
    json!({"response_action":"update","view":view})
}
fn errors(block: &str, text: &str) -> Value {
    json!({"response_action":"errors","errors":{block:clip(text,180)}})
}
fn request_error(error: &anyhow::Error) -> &'static str {
    let message = error.to_string().to_lowercase();
    if message.contains("workflow changed") {
        "The workflow changed since your preview. Review the updated details before starting."
    } else if message.contains("channel") {
        "The update channel is missing or changed. Ask an administrator to check the Slack connector, then review the details again."
    } else if message.contains("worker image") || message.contains("build factory-worker") {
        "Factory's worker is unavailable. Ask an administrator to build the configured worker image, then try again."
    } else if message.contains("repository access") || message.contains("operator required") {
        "You do not have permission to run work in this repository. Select an authorized repository or ask an administrator for access."
    } else if message.contains("ticket") || message.contains("jira") {
        "The ticket could not be loaded. Check its key or link and the ticket connector's access. Your inputs have been kept."
    } else {
        "The request could not be verified. Check the link and repository connector's access, then try again. Your inputs have been kept."
    }
}
fn field(values: &Value, name: &str) -> String {
    let v = &values[name]["value"];
    v["selected_option"]["value"]
        .as_str()
        .or(v["value"].as_str())
        .unwrap_or("")
        .trim()
        .to_owned()
}
fn input(name: &str, label: &str, placeholder: &str, value: &str, optional: bool) -> Value {
    let mut element = json!({"type":"plain_text_input","action_id":"value","max_length":2048,"placeholder":plain(placeholder)});
    if !value.is_empty() {
        element["initial_value"] = json!(value);
    }
    json!({"type":"input","block_id":name,"label":plain(label),"optional":optional,"element":element})
}
fn identity<'a>(app: &'a App, user: &str) -> Result<&'a Identity> {
    app.identities
        .iter()
        .find(|i| i.subject == format!("slack:{user}"))
        .context(
            "Your Slack account is not connected to Factory. Ask an administrator to grant access.",
        )
}
pub fn verify(secret: &str, headers: &HeaderMap, body: &[u8], now: i64) -> bool {
    let stamp = headers
        .get("X-Slack-Request-Timestamp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(time) = stamp.parse::<i64>().ok() else {
        return false;
    };
    if now.abs_diff(time) > 300 {
        return false;
    }
    let signature = headers
        .get("X-Slack-Signature")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("v0="))
        .unwrap_or("");
    let mut signed = format!("v0:{stamp}:").into_bytes();
    signed.extend_from_slice(body);
    verify_hmac(secret, &signed, signature)
}
async fn workspace(app: &App) -> Result<String> {
    let c = crate::connectors::load(&app.store, &app.executor.secret, "slack").await?;
    c.and_then(|c| c.values.get("team_id").cloned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("FACTORY_SLACK_TEAM_ID")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .context("Configure the Slack workspace ID before using Factory in Slack.")
}
async fn rpc(app: &App, method: &str, payload: Value) -> Result<Value> {
    let token = crate::connectors::credential(
        &app.store,
        &app.executor.secret,
        "slack",
        "token",
        "FACTORY_SLACK_BOT_TOKEN",
    )
    .await?;
    let base = "https://slack.com/api";
    #[cfg(test)]
    let base = TEST_SLACK_URL.get().map(String::as_str).unwrap_or(base);
    let response = app
        .http
        .post(format!("{base}/{method}"))
        .timeout(Duration::from_secs(2))
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await
        .context("Slack could not be reached")?;
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        let seconds = response
            .headers()
            .get("Retry-After")
            .and_then(|s| s.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(60)
            .clamp(1, 86400);
        return Err(SlackBackoff(seconds).into());
    }
    let value: Value = response
        .error_for_status()
        .context("Slack request failed")?
        .json()
        .await?;
    ensure!(
        value["ok"] == true,
        "Slack could not complete this action. Check app permissions and channel membership."
    );
    Ok(value)
}
async fn catalog(app: &App) -> Result<BTreeMap<String, Workflow>> {
    let mut all = if app.local_configuration {
        app.snapshot.workflows.clone()
    } else {
        crate::releases::catalog(&app.store).await?
    };
    all.retain(|_, w| {
        !w.phases
            .iter()
            .flat_map(|p| p.inputs.values())
            .any(|v| v.starts_with("parent."))
    });
    Ok(all)
}
fn is_review(w: &Workflow) -> bool {
    w.phases
        .iter()
        .flat_map(|p| &p.tasks)
        .any(|t| t.uses == "pull_request.fetch")
}
fn workflow_label(w: &Workflow) -> String {
    match w.id.as_str() {
        "pr-review" => "Review a pull request".into(),
        "research-plan" => "Research and implement a ticket".into(),
        "demo" => "Demo (no model calls)".into(),
        _ => clip(&w.id.replace('-', " "), 70),
    }
}
fn consequences(workflows: &BTreeMap<String, Workflow>, root: &str) -> String {
    let mut seen = std::collections::BTreeSet::new();
    let mut todo = vec![root.to_owned()];
    let mut perms = std::collections::BTreeSet::new();
    let mut gates = false;
    while let Some(id) = todo.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(w) = workflows.get(&id) {
            for p in &w.phases {
                gates |= p.gate.is_some();
                perms.extend(p.permissions.iter().map(String::as_str));
            }
            todo.extend(w.follow_ups.iter().map(|f| f.workflow.clone()));
        }
    }
    let mut actions = vec!["Runs the selected workflow and its configured follow-ups.".to_owned()];
    if perms.contains("repository.branch.write") {
        actions.push("Can commit changes and push to the work or PR branch.".into());
    }
    if perms.contains("pull_request.create") {
        actions.push("Can open a pull request.".into());
    }
    if perms.contains("pull_request.comment") {
        actions.push("Can publish reviews and comments.".into());
    }
    if gates {
        actions.push("Pauses at configured human approvals.".into());
    }
    actions.join(" ")
}
async fn workflow_snapshot(app: &App, id: &str) -> Result<crate::config::Snapshot> {
    if app.local_configuration {
        Ok(app.snapshot.clone())
    } else {
        crate::releases::active(&app.store, &app.platform, id).await
    }
}
fn form(id: Uuid, w: &Workflow, data: &Value, error: Option<&str>) -> Value {
    let mut blocks = vec![section(&w.description)];
    if let Some(e) = error {
        blocks.push(section(e));
    }
    if is_review(w) {
        blocks.push(input(
            "reference",
            "Pull request link",
            "https://github.com/owner/repo/pull/123",
            data["reference"].as_str().unwrap_or(""),
            false,
        ));
        blocks.push(input(
            "ticket",
            "Related ticket (optional)",
            "TEAM-42 or a ticket link",
            data["ticket"].as_str().unwrap_or(""),
            true,
        ));
    } else {
        blocks.push(input(
            "reference",
            "Ticket key or link",
            "TEAM-42 or an issue link",
            data["reference"].as_str().unwrap_or(""),
            false,
        ));
        let mut select = json!({"type":"external_select","action_id":"value","min_query_length":0,"placeholder":plain("Find a repository or paste its HTTPS URL")});
        if let Some(value) = data["repository"].as_str().filter(|s| !s.is_empty()) {
            select["initial_option"] = json!({"text":plain(&clip(value,75)),"value":value});
        }
        blocks.push(json!({"type":"input","block_id":"repository","label":plain("Repository"),"element":select}));
    }
    blocks.push(section("Next, Factory checks access and shows the resolved work and update channel. Nothing starts until you confirm."));
    modal(
        id,
        "factory_input",
        "Run workflow",
        blocks,
        Some("Review details"),
    )
}

/// Acknowledge promptly. Provider lookups and submissions are durable background work.
pub async fn hook(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let started = std::time::Instant::now();
    match tokio::time::timeout(Duration::from_millis(2800), hook_inner(app, headers, body)).await {
        Ok(response) => {
            tracing::info!(
                status = response.status().as_u16(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "Slack callback completed"
            );
            response
        }
        Err(_) => {
            tracing::warn!("Slack callback exceeded response deadline");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}
async fn hook_inner(app: Arc<App>, headers: HeaderMap, body: Bytes) -> Response {
    let secret = crate::connectors::credential(
        &app.store,
        &app.executor.secret,
        "slack",
        "signing_secret",
        "FACTORY_SLACK_SIGNING_SECRET",
    )
    .await;
    let Ok(secret) = secret else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !verify(&secret, &headers, &body, Utc::now().timestamp()) {
        tracing::warn!("Slack callback signature or timestamp rejected");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(form_url) = reqwest::Url::parse(&format!(
        "https://callback.invalid/?{}",
        String::from_utf8_lossy(&body)
    )) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let form: BTreeMap<String, String> = form_url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let payload = form
        .get("payload")
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    let team = payload
        .as_ref()
        .and_then(|p| p["team"]["id"].as_str())
        .or_else(|| form.get("team_id").map(String::as_str))
        .unwrap_or("");
    match workspace(&app).await { Ok(expected) if team==expected=>(), _=>return Json(json!({"response_type":"ephemeral","text":"Factory is not configured for this Slack workspace. Ask an administrator to check the workspace ID."})).into_response() }
    if payload.is_none()
        && form
            .get("text")
            .is_some_and(|s| s.starts_with("approve ") || s.starts_with("reject "))
    {
        return crate::api::legacy_slack(State(app), headers, body)
            .await
            .into_response();
    }
    let result = if let Some(p) = &payload {
        interaction(&app, p).await
    } else {
        open_launcher(&app, &form).await
    };
    match result {
        Ok(v) => Json(v).into_response(),
        Err(_) => {
            tracing::warn!(
                interaction = payload
                    .as_ref()
                    .and_then(|p| p["type"].as_str())
                    .filter(|s| matches!(
                        *s,
                        "view_submission" | "block_actions" | "block_suggestion"
                    ))
                    .unwrap_or("command_or_unknown"),
                "Slack action could not be completed; payload and provider errors withheld"
            );
            // Never echo provider errors or repository content into an unauthorized surface.
            let view = payload.as_ref().filter(|p| p["type"] == "view_submission");
            if let Some(p) = view {
                let block = match p["view"]["callback_id"].as_str().unwrap_or("") {
                    "factory_choose" => "workflow",
                    "factory_input" => "reference",
                    "factory_approval" => "decision",
                    _ => "confirm",
                };
                Json(errors(block,"This action is no longer available or you do not have access. Close this dialog and open Factory again.")).into_response()
            } else {
                if let Some(p) = &payload {
                    if let Some(trigger) = p["trigger_id"].as_str() {
                        let _=rpc(&app,"views.open",json!({"trigger_id":trigger,"view":progress(Uuid::new_v4(),"This action is no longer available or you do not have access. Refresh the status message or ask a Factory administrator.")})).await;
                    }
                }
                Json(json!({"response_type":"ephemeral","text":"Unable to open Factory. Check your Factory access, app permissions, and Slack configuration, then try again."})).into_response()
            }
        }
    }
}
async fn open_launcher(app: &App, params: &BTreeMap<String, String>) -> Result<Value> {
    let user = params.get("user_id").context("user missing")?;
    let who = identity(app, user)?;
    ensure!(
        who.roles.iter().any(|r| r == "operator"),
        "operator required"
    );
    let all = catalog(app).await?;
    ensure!(
        !all.is_empty() && all.len() <= 100,
        "No launchable workflows"
    );
    let id = Uuid::new_v4();
    let view = modal(
        id,
        "factory_choose",
        "Run workflow",
        vec![
            section("Choose what you want Factory to do."),
            json!({"type":"input","block_id":"workflow","label":plain("Workflow"),"element":{"type":"static_select","action_id":"value","placeholder":plain("Choose a workflow"),"options":all.values().map(|w|json!({"text":plain(&workflow_label(w)),"value":w.id})).collect::<Vec<_>>()}}),
        ],
        Some("Continue"),
    );
    sqlx::query("INSERT INTO slack_sessions(id,team_id,user_id,channel_id,stage) VALUES($1,$2,$3,$4,'choose')")
        .bind(id).bind(params.get("team_id")).bind(user).bind(params.get("channel_id")).execute(&app.store.pool).await?;
    let opened = rpc(
        app,
        "views.open",
        json!({"trigger_id":params.get("trigger_id"),"view":view}),
    )
    .await?;
    sqlx::query("UPDATE slack_sessions SET view_id=$2 WHERE id=$1")
        .bind(id)
        .bind(opened["view"]["id"].as_str())
        .execute(&app.store.pool)
        .await?;
    Ok(json!({}))
}
async fn interaction(app: &App, p: &Value) -> Result<Value> {
    if p["type"] == "block_suggestion" {
        let user = p["user"]["id"].as_str().context("user missing")?;
        let who = identity(app, user)?;
        ensure!(
            p["block_id"] == "repository" && who.roles.iter().any(|r| r == "operator"),
            "unknown selector"
        );
        let query = p["value"].as_str().unwrap_or("").trim();
        let mut choices: Vec<String> = app
            .platform
            .repositories
            .keys()
            .chain(who.repositories.iter())
            .filter(|r| {
                r.as_str() != "*"
                    && who.access(r, "operator")
                    && r.to_lowercase().contains(&query.to_lowercase())
            })
            .cloned()
            .collect();
        if query.starts_with("https://") && query.len() <= 150 && who.access(query, "operator") {
            choices.push(query.into());
        }
        choices.sort();
        choices.dedup();
        return Ok(
            json!({"options":choices.into_iter().filter(|s|s.len()<=150).take(100).map(|s|json!({"text":plain(&clip(&s,75)),"value":s})).collect::<Vec<_>>()}),
        );
    }
    if p["type"] == "block_actions" {
        if p["actions"][0]["action_id"] == "factory_edit" {
            return edit_form(app, p).await;
        }
        return open_approval(app, p).await;
    }
    ensure!(p["type"] == "view_submission", "unsupported interaction");
    let id: Uuid = p["view"]["private_metadata"]
        .as_str()
        .context("session missing")?
        .parse()?;
    let user = p["user"]["id"].as_str().context("user missing")?;
    let who = identity(app, user)?;
    let mut tx = app.store.pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='150ms'")
        .execute(&mut *tx)
        .await?;
    let (stage,mut data,channel):(String,Value,String)=sqlx::query_as("SELECT stage,data,channel_id FROM slack_sessions WHERE id=$1 AND user_id=$2 AND team_id=$3 AND (view_id=$4 OR view_id IS NULL) AND expires_at>now() FOR UPDATE")
        .bind(id).bind(user).bind(p["team"]["id"].as_str()).bind(p["view"]["id"].as_str()).fetch_one(&mut *tx).await?;
    let stage = if stage == "render_pending" {
        data["render_stage"].as_str().unwrap_or("").to_owned()
    } else {
        stage
    };
    let values = &p["view"]["state"]["values"];
    let callback = p["view"]["callback_id"].as_str().unwrap_or("");
    let (next,response)=match callback {
        "factory_choose" if stage=="choose" => {
            ensure!(who.roles.iter().any(|r|r=="operator"),"operator required");
            let all=catalog(app).await?; let selected=field(values,"workflow"); let w=all.get(&selected).context("workflow unavailable")?;
            data=json!({"workflow":selected});
            ("input",update(form(id,w,&data,None)))
        },
        "factory_input" if stage=="input" => {
            for key in ["reference","repository","ticket"] { data[key]=json!(field(values,key)); }
            let all=catalog(app).await?; let w=all.get(data["workflow"].as_str().unwrap_or("")).context("workflow unavailable")?;
            if data["reference"].as_str().unwrap_or("").is_empty() { return Ok(errors("reference","Enter a ticket or pull request.")); }
            if is_review(w) && crate::pull_requests::parse_link(data["reference"].as_str().unwrap()).is_err() { return Ok(errors("reference","Enter a GitHub or Azure DevOps pull request link.")); }
            if !is_review(w) && !who.access(data["repository"].as_str().unwrap_or(""),"operator") { return Ok(errors("repository","Choose a repository you have permission to operate.")); }
            ("preview_pending",update(progress(id,"Checking the link, repository access, and update channel… Nothing has started yet.")))
        },
        "factory_confirm" if stage=="preview" => {
            let submission:Submission=serde_json::from_value(data["submission"].clone())?;
            ensure!(who.access(&submission.repository,"operator"),"operator required");
            ensure!(field(values,"confirm")=="start","confirmation required");
            ("submit_pending",update(progress(id,"Starting your workflow… You can close this dialog. Updates will appear in the configured channel.")))
        },
        "factory_approval" if stage=="approval" => {
            let job=app.store.job(serde_json::from_value(data["job"].clone())?).await?;
            ensure!(app.approver(who,&job.repository.id),"approver required");
            let expected=crate::connectors::slack_channel(&app.store,&app.executor.secret,&app.platform,&job.repository.id).await?;
            ensure!(channel==expected,"channel changed");
            let choice=field(values,"decision");
            if choice!="approve"&&choice!="reject" { return Ok(errors("decision","Choose whether to approve or reject.")); }
            let decision=Decision {event_id:format!("slack:{id}"),gate_id:serde_json::from_value(data["gate"].clone())?,artifact_digest:data["digest"].as_str().context("digest missing")?.into(),approve:choice=="approve"};
            let result=app.store.mutate(job.id,|j,c|j.decide(&decision,&who.subject,"slack",Utc::now(),c)).await;
            if result.is_err() {
                let fresh=app.store.job(job.id).await?;
                let gate=fresh.gates.iter().find(|g|g.id==decision.gate_id).context("gate missing")?;
                if gate.decision_id.as_deref()!=Some(&decision.event_id) {
                    if gate.status!="pending" { return Ok(update(progress(id,&format!("This approval is already {}{}. Check the status message for the current outcome.",gate.status,gate.decided_by.as_ref().map(|u|format!(" by {u}")).unwrap_or_default())))); }
                    if gate.deadline<=Utc::now() { return Ok(update(progress(id,"This approval has expired. No decision was recorded."))); }
                    return Ok(errors("decision","Could not confirm the decision. Please try again in this dialog."));
                }
            }
            ("decided",update(progress(id,if decision.approve {"Approved. Factory will continue. The status message will update shortly."}else{"Rejected. This run has stopped."})))
        },
        _ => return Ok(update(progress(id,"This request has already been handled or expired. Check its status message for the latest outcome.")))
    };
    sqlx::query("UPDATE slack_sessions SET stage=$2,data=$3,view_id=$4,available_at=now(),tries=0 WHERE id=$1")
        .bind(id).bind(next).bind(data).bind(p["view"]["id"].as_str()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(response)
}

async fn open_approval(app: &App, p: &Value) -> Result<Value> {
    let action = &p["actions"][0];
    ensure!(
        action["action_id"] == "factory_review_gate",
        "unknown action"
    );
    let gate: Uuid = action["value"].as_str().context("gate missing")?.parse()?;
    let document: Value =
        sqlx::query_scalar("SELECT document FROM jobs WHERE document->'gates' @> $1::jsonb")
            .bind(json!([{"id":gate}]))
            .fetch_one(&app.store.pool)
            .await?;
    let job: Job = serde_json::from_value(document)?;
    let user = p["user"]["id"].as_str().context("user missing")?;
    ensure!(
        app.approver(identity(app, user)?, &job.repository.id),
        "approver required"
    );
    let channel = crate::connectors::slack_channel(
        &app.store,
        &app.executor.secret,
        &app.platform,
        &job.repository.id,
    )
    .await?;
    ensure!(
        p["channel"]["id"].as_str() == Some(&channel),
        "wrong channel"
    );
    let g = job
        .gates
        .iter()
        .find(|g| g.id == gate)
        .context("gate missing")?;
    let id = Uuid::new_v4();
    if g.status != "pending" || g.deadline <= Utc::now() {
        rpc(app,"views.open",json!({"trigger_id":p["trigger_id"],"view":progress(id,&format!("This approval is {}. {}",if g.deadline<=Utc::now()&&g.status=="pending" {"expired"}else{&g.status},g.decided_by.as_ref().map(|u|format!("Decision by {u}.")).unwrap_or_default()))})).await?;
        return Ok(json!({}));
    }
    let data = json!({"job":job.id,"gate":g.id,"digest":g.artifact_digest});
    // Insert before opening; the background worker waits until a view ID exists.
    sqlx::query("INSERT INTO slack_sessions(id,team_id,user_id,channel_id,stage,data) VALUES($1,$2,$3,$4,'approval_pending',$5)")
        .bind(id).bind(p["team"]["id"].as_str()).bind(user).bind(channel).bind(data).execute(&app.store.pool).await?;
    let opened=rpc(app,"views.open",json!({"trigger_id":p["trigger_id"],"view":progress(id,"Loading the exact report attached to this approval…")})).await?;
    sqlx::query("UPDATE slack_sessions SET view_id=$2 WHERE id=$1")
        .bind(id)
        .bind(opened["view"]["id"].as_str())
        .execute(&app.store.pool)
        .await?;
    Ok(json!({}))
}

async fn edit_form(app: &App, p: &Value) -> Result<Value> {
    let id: Uuid = p["view"]["private_metadata"]
        .as_str()
        .context("session missing")?
        .parse()?;
    let user = p["user"]["id"].as_str().context("user missing")?;
    identity(app, user)?;
    let mut tx = app.store.pool.begin().await?;
    let data:Value=sqlx::query_scalar("SELECT data FROM slack_sessions WHERE id=$1 AND user_id=$2 AND team_id=$3 AND view_id=$4 AND (stage='preview' OR (stage='render_pending' AND data->>'render_stage'='preview')) AND expires_at>now() FOR UPDATE")
        .bind(id).bind(user).bind(p["team"]["id"].as_str()).bind(p["view"]["id"].as_str()).fetch_one(&mut *tx).await?;
    let all = catalog(app).await?;
    let w = all
        .get(data["workflow"].as_str().context("workflow missing")?)
        .context("workflow missing")?;
    rpc(
        app,
        "views.update",
        json!({"view_id":p["view"]["id"],"hash":p["view"]["hash"],"view":form(id,w,&data,None)}),
    )
    .await?;
    sqlx::query("UPDATE slack_sessions SET stage='input' WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({}))
}

fn repository_alias(app: &App, who: &Identity, url: &str, provider: &str) -> String {
    app.platform
        .repositories
        .iter()
        .find(|(id, r)| {
            who.access(id, "operator")
                && r.provider == provider.trim_end_matches("_pr")
                && if r.provider == "github" {
                    crate::providers::github_repository_url(&r.url)
                        == crate::providers::github_repository_url(url)
                } else {
                    r.url.trim_end_matches('/') == url.trim_end_matches('/')
                }
        })
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| url.into())
}
async fn preview(app: &App, user: &str, data: &mut Value) -> Result<Value> {
    let who = identity(app, user)?;
    let workflow = data["workflow"].as_str().context("workflow missing")?;
    let snapshot = workflow_snapshot(app, workflow).await?;
    let w = snapshot
        .workflows
        .get(workflow)
        .context("workflow missing")?;
    ensure!(
        !w.phases
            .iter()
            .flat_map(|p| p.inputs.values())
            .any(|s| s.starts_with("parent.")),
        "follow-up only"
    );
    let reference = data["reference"].as_str().context("reference missing")?;
    let submission = if is_review(w) {
        let (url, provider, number) = crate::pull_requests::parse_link(reference)?;
        let repository = repository_alias(app, who, &url, &provider);
        ensure!(
            who.access(&repository, "operator"),
            "repository access required"
        );
        let config = crate::repositories::resolve(
            &app.store,
            &app.executor.secret,
            &app.platform,
            &app.http,
            &repository,
        )
        .await?;
        let token = crate::connectors::repository_token(
            &app.store,
            &app.executor.secret,
            &repository,
            &config,
            false,
        )
        .await?
        .context("repository credential missing")?;
        let client = crate::pull_requests::Provider {
            http: &app.http,
            kind: &config.provider,
            token: &token,
            api: &config.api_url,
        };
        let metadata = client.metadata(&number).await?;
        crate::providers::pull_request_context(&config.provider, &config.url, &metadata)?;
        let ticket = data["ticket"].as_str().filter(|s| !s.is_empty());
        if let Some(ticket) = ticket {
            crate::pull_requests::ticket(app, ticket, &repository).await?;
        }
        let mut issue = crate::pull_requests::metadata_issue(
            &config.provider,
            &number,
            reference,
            &metadata,
            None,
        )?;
        issue.ticket = ticket.map(|s| json!({"reference":s}));
        Submission {
            workflow: workflow.into(),
            repository,
            issue,
        }
    } else {
        let repository = data["repository"]
            .as_str()
            .context("repository missing")?
            .to_owned();
        ensure!(
            who.access(&repository, "operator"),
            "repository access required"
        );
        let config = crate::repositories::resolve(
            &app.store,
            &app.executor.secret,
            &app.platform,
            &app.http,
            &repository,
        )
        .await?;
        let issue = if config.provider == "fixture" {
            Issue {
                provider: "fixture".into(),
                key: clip(reference, 120),
                title: clip(reference, 500),
                body: "Slack fixture run; no model calls.".into(),
                url: None,
                ticket: None,
            }
        } else {
            let ticket = crate::pull_requests::ticket(app, reference, &repository).await?;
            // Hydrated ticket content is pinned in the preview; no model interprets this input.
            Issue {
                provider: "manual".into(),
                key: if reference.len() <= 120 {
                    reference.into()
                } else {
                    format!("ticket-{}", &hash(reference)[..24])
                },
                title: clip(
                    ticket["title"].as_str().context("ticket title missing")?,
                    500,
                ),
                body: ticket["description"].as_str().unwrap_or("").into(),
                url: if reference.starts_with("https://") {
                    Some(reference.into())
                } else {
                    None
                },
                ticket: Some(ticket),
            }
        };
        Submission {
            workflow: workflow.into(),
            repository,
            issue,
        }
    };
    let channel = crate::connectors::slack_channel(
        &app.store,
        &app.executor.secret,
        &app.platform,
        &submission.repository,
    )
    .await?;
    ensure!(!channel.is_empty(), "channel missing");
    let id: Uuid = serde_json::from_value(data["session"].clone())?;
    let explanation = consequences(&snapshot.workflows, workflow);
    let view = modal(
        id,
        "factory_confirm",
        "Ready to start",
        vec![
            section(&format!(
                "{}\n{}\nRepository: {}",
                workflow_label(w),
                submission.issue.title,
                submission.repository
            )),
            json!({"type":"section","text":{"type":"mrkdwn","text":format!("Updates will appear in <#{channel}>." )}}),
            section(&explanation),
            json!({"type":"actions","elements":[button("Edit details","factory_edit",&id.to_string())]}),
            json!({"type":"input","block_id":"confirm","label":plain("Start this workflow?"),"element":{"type":"radio_buttons","action_id":"value","options":[{"text":plain("Start with the behavior described above"),"value":"start"}]}}),
        ],
        Some(if is_review(w) {
            "Start review"
        } else {
            "Start workflow"
        }),
    );
    data["submission"] = serde_json::to_value(submission)?;
    data["destination"] = json!(channel);
    data["definition_hash"] = json!(snapshot.definition_hash);
    Ok(view)
}
fn portal(path: &str) -> Option<String> {
    let base = std::env::var("FACTORY_PORTAL_URL").ok()?;
    let url = reqwest::Url::parse(&base).ok()?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(format!("{}#{path}", base.trim_end_matches('/')))
}
fn link(label: &str, url: &str) -> Value {
    json!({"type":"button","text":plain(label),"url":url})
}
async fn approval_view(app: &App, id: Uuid, user: &str, data: &Value) -> Result<Value> {
    let job = app
        .store
        .job(serde_json::from_value(data["job"].clone())?)
        .await?;
    ensure!(
        app.approver(identity(app, user)?, &job.repository.id),
        "approver required"
    );
    let gate = job
        .gates
        .iter()
        .find(|g| Some(g.id.to_string()) == data["gate"].as_str().map(String::from))
        .context("gate missing")?;
    ensure!(
        gate.status == "pending"
            && gate.deadline > Utc::now()
            && data["digest"] == gate.artifact_digest,
        "approval expired"
    );
    let attempt = job
        .attempts
        .iter()
        .find(|a| a.id == gate.attempt_id)
        .context("attempt missing")?;
    let mut blocks = vec![section(&format!(
        "Review {} · {}\n{}\nExpires {} UTC",
        gate.phase,
        job.issue.key,
        job.issue.title,
        gate.deadline.format("%d %b %H:%M")
    ))];
    let mut budget = 12_000usize;
    for artifact in attempt.artifacts.values() {
        let bytes = app.blobs.get(&artifact.sha256).await?;
        let report = String::from_utf8_lossy(&bytes);
        let limit = budget.min(8000);
        let truncated = report.chars().count() > limit;
        if truncated && portal(&format!("report/{}/{}", job.id, artifact.id)).is_none() {
            bail!("Full report requires portal URL");
        }
        blocks.push(section(&format!(
            "Report: {}{}",
            artifact.name,
            if truncated {
                " (excerpt — open the full report before deciding)"
            } else {
                ""
            }
        )));
        let chars: Vec<char> = report.chars().take(limit).collect();
        for chunk in chars.chunks(2800) {
            blocks.push(section(&chunk.iter().collect::<String>()));
        }
        budget = budget.saturating_sub(chars.len());
        if let Some(url) = portal(&format!("report/{}/{}", job.id, artifact.id)) {
            blocks.push(json!({"type":"actions","elements":[link("Open full report",&url)]}));
        }
    }
    let next = job.snapshot.workflows[&job.workflow]
        .phases
        .get(job.phase_index + 1)
        .map(|p| p.id.as_str())
        .unwrap_or("workflow completion");
    blocks.push(section(&format!(
        "Approve: continue to {next}. Reject: stop this run.\n{}",
        consequences(&job.snapshot.workflows, &job.workflow)
    )));
    blocks.push(json!({"type":"input","block_id":"decision","label":plain("After reviewing the report"),"element":{"type":"radio_buttons","action_id":"value","options":[{"text":plain("Approve and continue"),"value":"approve"},{"text":plain("Reject and stop"),"value":"reject"}]}}));
    ensure!(blocks.len() <= 95, "Too many reports for a Slack dialog");
    Ok(modal(
        id,
        "factory_approval",
        "Review approval",
        blocks,
        Some("Confirm decision"),
    ))
}

/// One durable operation per call; locks serialize retries and competing servers.
pub async fn process_one(app: &App) -> Result<bool> {
    let mut tx = app.store.pool.begin().await?;
    let row:Option<(Uuid,String,String,String,Value)>=sqlx::query_as("SELECT id,stage,user_id,view_id,data FROM slack_sessions WHERE stage IN ('preview_pending','submit_pending','approval_pending','render_pending') AND view_id IS NOT NULL AND available_at<=now() AND expires_at>now() ORDER BY available_at LIMIT 1 FOR UPDATE SKIP LOCKED")
        .fetch_optional(&mut *tx).await?;
    let Some((id, stage, user, view_id, mut data)) = row else {
        return Ok(false);
    };
    data["session"] = json!(id);
    let outcome:Result<(String,Value)>=async {
        match stage.as_str() {
            "preview_pending"=>Ok(("preview".into(),preview(app,&user,&mut data).await?)),
            "approval_pending"=>Ok(("approval".into(),approval_view(app,id,&user,&data).await?)),
            "submit_pending"=>{
                let who=identity(app,&user)?;
                let submission:Submission=serde_json::from_value(data["submission"].clone())?;
                ensure!(who.access(&submission.repository,"operator"),"operator required");
                let snapshot=workflow_snapshot(app,&submission.workflow).await?;
                ensure!(data["definition_hash"]==snapshot.definition_hash,"workflow changed; preview again");
                let channel=crate::connectors::slack_channel(&app.store,&app.executor.secret,&app.platform,&submission.repository).await?;
                ensure!(data["destination"]==channel,"update channel changed; preview again");
                let job=app.submit_snapshot(&format!("slack:{id}"),submission,&who.subject,Some(snapshot)).await?;
                track(&app.store,job.root_id,&channel).await?;
                data["job"]=json!(job.id);
                Ok(("started".into(),progress(id,&format!("Started: {}\nUpdates are in the configured Slack channel. You can close this dialog.",job.issue.title))))
            },
            "render_pending"=>Ok((data["render_stage"].as_str().context("render stage missing")?.into(),data["render_view"].clone())),
            _=>unreachable!()
        }
    }.await;
    let (next, view) = match outcome {
        Ok(result) => result,
        Err(error) => {
            #[cfg(test)]
            eprintln!("Slack test operation {stage}: {error:#}");
            if stage == "approval_pending" {
                ("failed".into(),progress(id,"The report could not be loaded or this approval is no longer available. Close this dialog and retry from the status message. No decision was recorded."))
            } else if let Some(w) = catalog(app)
                .await?
                .get(data["workflow"].as_str().unwrap_or(""))
            {
                (
                    "input".into(),
                    form(id, w, &data, Some(request_error(&error))),
                )
            } else {
                (
                    "failed".into(),
                    progress(
                        id,
                        "This workflow is no longer available. Open /factory again.",
                    ),
                )
            }
        }
    };
    // Persist the result before calling Slack; a lost response only repeats rendering,
    // never provider reads or a successful submission with changed input.
    data["render_stage"] = json!(next);
    data["render_view"] = view;
    sqlx::query("UPDATE slack_sessions SET stage='render_pending',data=$2,available_at=now()+interval '3 seconds' WHERE id=$1").bind(id).bind(&data).execute(&mut *tx).await?;
    tx.commit().await?;
    let mut render_tx = app.store.pool.begin().await?;
    let current: Option<Value> = sqlx::query_scalar(
        "SELECT data FROM slack_sessions WHERE id=$1 AND stage='render_pending' FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *render_tx)
    .await?;
    if current.as_ref() != Some(&data) {
        return Ok(true);
    }
    let rendered = rpc(
        app,
        "views.update",
        json!({"view_id":view_id,"view":data["render_view"]}),
    )
    .await;
    if rendered.is_ok() {
        sqlx::query("UPDATE slack_sessions SET stage=$2 WHERE id=$1 AND stage='render_pending' AND data->>'render_stage'=$2").bind(id).bind(&next).execute(&mut *render_tx).await?;
    } else {
        let delay = rendered
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<SlackBackoff>())
            .map_or(60, |e| e.0) as i32;
        sqlx::query("UPDATE slack_sessions SET tries=tries+1,available_at=now()+make_interval(secs=>$3),stage=CASE WHEN tries>=4 THEN $2 ELSE stage END WHERE id=$1 AND stage='render_pending'").bind(id).bind(&next).bind(delay).execute(&mut *render_tx).await?;
    }
    render_tx.commit().await?;
    Ok(true)
}

pub async fn track(store: &crate::store::Store, root: Uuid, channel: &str) -> Result<()> {
    sqlx::query("INSERT INTO slack_messages(root_id,channel_id) VALUES($1,$2) ON CONFLICT(root_id) DO UPDATE SET finished=false,available_at=now()")
        .bind(root).bind(channel).execute(&store.pool).await?;
    Ok(())
}
fn status(job: &Job) -> String {
    match job.status {
        JobStatus::Queued => format!("Queued · {}", job.phase().id),
        JobStatus::Running => format!("Running · {}", job.phase().id),
        JobStatus::AwaitingApproval => format!("Waiting for {} approval", job.phase().id),
        JobStatus::Succeeded => "Completed".into(),
        JobStatus::Rejected => "Rejected · this run has stopped".into(),
        JobStatus::Cancelled => "Cancelled".into(),
        JobStatus::TimedOut => "Timed out · view details".into(),
        JobStatus::Failed => "Failed · view details for the cause".into(),
    }
}
pub fn card(job: &Job) -> Value {
    let state = status(job);
    let mut blocks = vec![
        json!({"type":"header","text":plain(&clip(&job.issue.title,150))}),
        section(&format!(
            "{} · {}\n{}\nStarted by {}",
            job.workflow, job.repository.id, state, job.requested_by
        )),
    ];
    let mut buttons = vec![];
    if let Some(g) = job
        .gates
        .iter()
        .rev()
        .find(|g| g.status == "pending" && g.deadline > Utc::now())
    {
        let slack_gate = job
            .phase()
            .gate
            .as_ref()
            .is_some_and(|d| d.channels.iter().any(|c| c == "slack"));
        blocks.push(section(&format!(
            "{} is ready to review. Expires {} UTC.",
            g.phase,
            g.deadline.format("%d %b %H:%M")
        )));
        if slack_gate {
            buttons.push(button(
                "Review approval",
                "factory_review_gate",
                &g.id.to_string(),
            ));
        } else {
            blocks.push(section("This workflow requires approval in the portal."));
        }
    }
    if let Some(g) = job.gates.iter().rev().find(|g| g.decided_by.is_some()) {
        blocks.push(section(&format!(
            "{}: {} by {} at {}",
            g.phase,
            g.status,
            g.decided_by.as_deref().unwrap_or(""),
            g.decided_at
                .map(|d| d.format("%d %b %H:%M UTC").to_string())
                .unwrap_or_default()
        )));
    }
    let pr = job
        .attempts
        .iter()
        .rev()
        .filter_map(|a| a.result.as_ref())
        .find_map(|r| r.pull_request.as_deref())
        .or_else(|| {
            if job.issue.provider.ends_with("_pr") {
                job.issue.url.as_deref()
            } else {
                None
            }
        });
    if let Some(url) = pr.filter(|url| url.starts_with("https://")) {
        buttons.push(link("Open pull request", url));
    }
    if let Some(url) = portal(&format!("job/{}", job.id)) {
        buttons.push(link("View details", &url));
    }
    if !buttons.is_empty() {
        blocks.push(json!({"type":"actions","elements":buttons}));
    }
    json!({"text":slack_text(&format!("{} · {} · {}",job.issue.title,job.repository.id,state)),"blocks":blocks,"unfurl_links":false,"unfurl_media":false,"parse":"none"})
}
pub async fn sync_one(app: &App) -> Result<bool> {
    let mut tx = app.store.pool.begin().await?;
    let row:Option<MessageRow>=sqlx::query_as("SELECT root_id,channel_id,message_ts,rendered_hash,milestone_hash FROM slack_messages WHERE NOT finished AND available_at<=now() ORDER BY available_at LIMIT 1 FOR UPDATE SKIP LOCKED").fetch_optional(&mut *tx).await?;
    let Some(MessageRow {
        root_id: root,
        channel_id: channel,
        message_ts: ts,
        rendered_hash: old,
        milestone_hash: milestone,
    }) = row
    else {
        return Ok(false);
    };
    // A follow-up remains part of the same Slack run and never appears completed early.
    let document:Value=sqlx::query_scalar("SELECT document FROM jobs WHERE document->>'root_id'=$1 ORDER BY (document->>'depth')::int DESC,created_at DESC LIMIT 1").bind(root.to_string()).fetch_one(&mut *tx).await?;
    let job: Job = serde_json::from_value(document)?;
    let mut message = card(&job);
    let digest = hash(serde_json::to_vec(&message)?);
    message["channel"] = json!(channel);
    let significant = job.status.terminal() || job.status == JobStatus::AwaitingApproval;
    let milestone_id = hash(format!("{}:{:?}:{}", job.id, job.status, job.phase_index));
    let operation:Result<()>=async {
        let message_ts=if let Some(ts)=&ts {
            if old.as_deref()!=Some(&digest) { message["ts"]=json!(ts); rpc(app,"chat.update",message.clone()).await?; }
            ts.clone()
        }else{
            message["client_msg_id"]=json!(root);
            let response=rpc(app,"chat.postMessage",message.clone()).await?;
            let ts=response["ts"].as_str().context("Slack message id missing")?.to_owned();
            sqlx::query("UPDATE slack_messages SET message_ts=$2 WHERE root_id=$1").bind(root).bind(&ts).execute(&mut *tx).await?;
            ts
        };
        if ts.is_some()&&significant&&milestone.as_deref()!=Some(&milestone_id) {
            rpc(app,"chat.postMessage",json!({"channel":channel,"thread_ts":message_ts,"text":slack_text(&format!("{} · {}",job.issue.title,status(&job))),"client_msg_id":format!("{}",Uuid::from_bytes(hex::decode(&milestone_id[..32])?.try_into().unwrap())),"unfurl_links":false,"parse":"none"})).await?;
        }
        Ok(())
    }.await;
    if operation.is_ok() {
        sqlx::query("UPDATE slack_messages SET rendered_hash=$2,milestone_hash=$3,finished=$4,tries=0,available_at=now()+interval '5 seconds' WHERE root_id=$1")
            .bind(root).bind(digest).bind(if significant{Some(milestone_id)}else{milestone}).bind(job.status.terminal()).execute(&mut *tx).await?;
    } else {
        let delay = operation
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<SlackBackoff>())
            .map_or(0, |e| e.0) as i32;
        sqlx::query("UPDATE slack_messages SET tries=tries+1,available_at=now()+make_interval(secs=>GREATEST($2,LEAST(300,5*power(2,LEAST(tries,6))::int))) WHERE root_id=$1").bind(root).bind(delay).execute(&mut *tx).await?;
        tracing::warn!(%root,"Slack status delivery failed; will retry");
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    #[test]
    fn signatures_bind_timestamp_and_exact_body() {
        let body = b"team_id=T1&text=";
        let mut mac = Hmac::<Sha256>::new_from_slice(b"secret").unwrap();
        mac.update(b"v0:1000:");
        mac.update(body);
        let mut h = HeaderMap::new();
        h.insert("X-Slack-Request-Timestamp", "1000".parse().unwrap());
        h.insert(
            "X-Slack-Signature",
            format!("v0={}", hex::encode(mac.finalize().into_bytes()))
                .parse()
                .unwrap(),
        );
        assert!(verify("secret", &h, body, 1100));
        assert!(!verify("secret", &h, body, 1301));
        assert!(!verify("wrong", &h, body, 1100));
        assert!(!verify("secret", &h, b"changed", 1100));
        h.insert(
            "X-Slack-Request-Timestamp",
            i64::MIN.to_string().parse().unwrap(),
        );
        assert!(!verify("secret", &h, body, 1000));
    }
    #[test]
    fn text_inputs_and_radio_choices_are_read_without_trusting_other_fields() {
        let values = json!({"reference":{"value":{"value":" TEAM-42 "}},"decision":{"value":{"selected_option":{"value":"approve"}}}});
        assert_eq!(field(&values, "reference"), "TEAM-42");
        assert_eq!(field(&values, "decision"), "approve");
        assert_eq!(field(&values, "missing"), "");
    }
    #[test]
    fn untrusted_reports_remain_plain_text_and_unicode_is_bounded() {
        assert_eq!(
            section("<!channel> *approve me*")["text"]["type"],
            "plain_text"
        );
        assert_eq!(clip("😀😀😀", 2), "😀…");
    }
    #[test]
    fn consequences_include_follow_up_writes() {
        let platform =
            crate::config::Platform::load(std::path::Path::new("config/platform.yaml")).unwrap();
        let s = platform
            .snapshot(
                std::path::Path::new("workflows"),
                std::path::Path::new("prompts"),
            )
            .unwrap();
        let text = consequences(&s.workflows, "pr-review");
        assert!(text.contains("push"));
        assert!(text.contains("reviews"));
        let v = form(
            Uuid::new_v4(),
            &s.workflows["pr-review"],
            &json!({"reference":"https://github.com/a/b/pull/1"}),
            Some("Try again"),
        );
        assert_eq!(
            v["blocks"][2]["element"]["initial_value"],
            "https://github.com/a/b/pull/1"
        );
        assert!(!v.to_string().contains("Repository URL"));
    }
}
