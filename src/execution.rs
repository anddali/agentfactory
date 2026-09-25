use crate::{
    config::{Platform, Snapshot},
    model::*,
    store::Store,
};
use anyhow::{bail, ensure, Context, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio::process::Command;
use uuid::Uuid;

#[derive(Clone)]
pub struct Executor {
    pub mode: String,
    pub server_url: String,
    pub network: String,
    pub worker_binary: PathBuf,
    pub secret: String,
    pub ecs_cluster: String,
    pub ecs_network: String,
}
pub fn token(secret: &str, attempt: Uuid) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("factory-attempt:{attempt}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
pub fn verify_hmac(secret: &str, message: &[u8], signature: &str) -> bool {
    let Ok(bytes) = hex::decode(signature) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message);
    mac.verify_slice(&bytes).is_ok()
}
pub fn secure_equal(left: &str, right: &str) -> bool {
    // HMAC verification provides constant-time comparison without exposing token length-dependent prefixes.
    let mut mac = Hmac::<Sha256>::new_from_slice(b"factory-token-comparison").unwrap();
    mac.update(right.as_bytes());
    let expected = mac.finalize().into_bytes();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"factory-token-comparison").unwrap();
    mac.update(left.as_bytes());
    mac.verify_slice(&expected).is_ok()
}
async fn output(command: &mut Command) -> Result<std::process::Output> {
    command.kill_on_drop(true);
    Ok(
        tokio::time::timeout(Duration::from_secs(35), command.output())
            .await
            .context("executor command timed out")??,
    )
}
impl Executor {
    pub async fn pin(&self, mut snapshot: Snapshot, workflow: &str) -> Result<Snapshot> {
        // Pin every reachable workflow's image, including conditional follow-up jobs.
        let mut pending = vec![workflow.to_owned()];
        let mut visited = std::collections::BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            let w = snapshot.workflows.get(&id).context("unknown workflow")?;
            pending.extend(w.follow_ups.iter().map(|f| f.workflow.clone()));
            let profile = snapshot
                .workers
                .get_mut(&w.defaults.worker_profile)
                .context("unknown worker profile")?;
            if self.mode == "docker"
                && !profile.image.starts_with("sha256:")
                && !profile.image.contains("@sha256:")
            {
                let result = output(Command::new("docker").args([
                    "image",
                    "inspect",
                    "--format",
                    "{{.Id}}",
                    &profile.image,
                ]))
                .await?;
                ensure!(
                    result.status.success(),
                    "worker image is unavailable; build it before submitting jobs"
                );
                profile.image = String::from_utf8(result.stdout)?.trim().to_owned();
                ensure!(
                    profile.image.starts_with("sha256:"),
                    "worker image digest could not be resolved"
                );
            } else if self.mode == "ecs" {
                ensure!(
                    profile.image.contains("@sha256:"),
                    "ECS worker image must be pinned by digest"
                );
                let task = profile
                    .ecs_task_definition
                    .as_deref()
                    .context("ECS task definition required")?;
                ensure!(
                    task.rsplit(':')
                        .next()
                        .is_some_and(|s| s.parse::<u32>().is_ok()),
                    "ECS task definition must include an immutable revision"
                );
                let result = output(Command::new("aws").args([
                    "ecs",
                    "describe-task-definition",
                    "--task-definition",
                    task,
                    "--output",
                    "json",
                ]))
                .await?;
                ensure!(
                    result.status.success(),
                    "could not inspect ECS task definition"
                );
                let data: serde_json::Value = serde_json::from_slice(&result.stdout)?;
                let containers = data["taskDefinition"]["containerDefinitions"]
                    .as_array()
                    .context("invalid ECS task definition")?;
                ensure!(
                    containers
                        .iter()
                        .any(|c| c["name"] == "worker" && c["image"] == profile.image),
                    "ECS task definition does not use the pinned worker image"
                );
            } else if self.mode == "process" {
                profile.image = format!(
                    "binary-sha256:{}",
                    hash(
                        tokio::fs::read(&self.worker_binary)
                            .await
                            .context("build factory-worker first")?
                    )
                );
            }
        }
        snapshot.refresh_hash()?;
        Ok(snapshot)
    }
    fn env(&self, attempt: Uuid) -> BTreeMap<String, String> {
        let mut vars = BTreeMap::from([
            ("FACTORY_SERVER_URL".into(), self.server_url.clone()),
            ("FACTORY_ATTEMPT_ID".into(), attempt.to_string()),
            ("FACTORY_ATTEMPT_TOKEN".into(), token(&self.secret, attempt)),
        ]);
        if let Ok(headers) = std::env::var("OTEL_EXPORTER_OTLP_TRACES_HEADERS") {
            if !headers.is_empty() {
                vars.insert("OTEL_EXPORTER_OTLP_TRACES_HEADERS".into(), headers);
            }
        }
        vars
    }
    pub async fn start(&self, job: &Job, attempt: Uuid, platform: &Platform) -> Result<()> {
        if job.active(attempt, Utc::now()).is_err() {
            return Ok(());
        }
        let vars = self.env(attempt);
        let profile = &job.snapshot.workers[&job.snapshot.workflows[&job.workflow]
            .defaults
            .worker_profile];
        let name = format!("factory-{attempt}");
        match self.mode.as_str() {
            "docker" => {
                let existing = output(Command::new("docker").args([
                    "container",
                    "inspect",
                    "--format",
                    "{{.State.Status}}",
                    &name,
                ]))
                .await?;
                if existing.status.success() {
                    if String::from_utf8_lossy(&existing.stdout).trim() == "created" {
                        ensure!(
                            output(Command::new("docker").args(["start", &name]))
                                .await?
                                .status
                                .success(),
                            "could not start worker"
                        );
                    }
                    return Ok(());
                }
                let mut create = Command::new("docker");
                create.args([
                    "create",
                    "--name",
                    &name,
                    "--label",
                    "factory.managed=true",
                    "--network",
                    &self.network,
                    "--cpus",
                    &profile.cpus,
                    "--memory",
                    &profile.memory,
                    "--read-only",
                    "--tmpfs",
                    "/work:rw,exec,size=2g,uid=10001,gid=10001",
                    "--tmpfs",
                    "/tmp:rw,noexec,size=128m",
                    "--cap-drop",
                    "ALL",
                    "--security-opt",
                    "no-new-privileges",
                    "--pids-limit",
                    "256",
                ]);
                for (key, value) in vars {
                    create.args(["--env", &format!("{key}={value}")]);
                }
                create.arg(&profile.image);
                ensure!(
                    output(&mut create).await?.status.success(),
                    "could not create worker container"
                );
                ensure!(
                    output(Command::new("docker").args(["start", &name]))
                        .await?
                        .status
                        .success(),
                    "could not start worker container"
                );
            }
            "ecs" => {
                let request = json!({"cluster":self.ecs_cluster,"taskDefinition":profile.ecs_task_definition,"launchType":"FARGATE","clientToken":attempt.to_string(),"startedBy":attempt.to_string(),"networkConfiguration":serde_json::from_str::<serde_json::Value>(&self.ecs_network)?,"overrides":{"containerOverrides":[{"name":"worker","environment":vars.iter().map(|(k,v)|json!({"name":k,"value":v})).collect::<Vec<_>>()}]}});
                let result = output(Command::new("aws").args([
                    "ecs",
                    "run-task",
                    "--cli-input-json",
                    &request.to_string(),
                    "--output",
                    "json",
                ]))
                .await?;
                ensure!(result.status.success(), "ECS dispatch failed");
                let response: serde_json::Value = serde_json::from_slice(&result.stdout)?;
                ensure!(
                    response["failures"]
                        .as_array()
                        .is_some_and(|f| f.is_empty())
                        && response["tasks"].as_array().is_some_and(|t| !t.is_empty()),
                    "ECS rejected task placement"
                );
            }
            "process" => {
                ensure!(
                    platform.allow_fixture && job.repository.provider == "fixture",
                    "process executor is restricted to trusted fixture workflows"
                );
                let mut command = Command::new(&self.worker_binary);
                command.envs(vars).stdin(std::process::Stdio::null());
                command.spawn().context("launching development worker")?;
            }
            _ => bail!("unsupported execution adapter"),
        }
        Ok(())
    }
    pub async fn stop(&self, attempt: Uuid) -> Result<()> {
        match self.mode.as_str() {
            "docker" => {
                let name = format!("factory-{attempt}");
                let result = output(Command::new("docker").args(["rm", "--force", &name])).await?;
                ensure!(
                    result.status.success()
                        || String::from_utf8_lossy(&result.stderr).contains("No such container"),
                    "worker removal failed"
                );
            }
            "ecs" => {
                let result = output(Command::new("aws").args([
                    "ecs",
                    "list-tasks",
                    "--cluster",
                    &self.ecs_cluster,
                    "--started-by",
                    &attempt.to_string(),
                    "--output",
                    "json",
                ]))
                .await?;
                ensure!(result.status.success(), "ECS reconciliation failed");
                let response: serde_json::Value = serde_json::from_slice(&result.stdout)?;
                for task in response["taskArns"]
                    .as_array()
                    .context("missing ECS tasks")?
                {
                    ensure!(
                        output(Command::new("aws").args([
                            "ecs",
                            "stop-task",
                            "--cluster",
                            &self.ecs_cluster,
                            "--task",
                            task.as_str().context("invalid task ARN")?,
                            "--reason",
                            "Factory phase ended or was cancelled"
                        ]))
                        .await?
                        .status
                        .success(),
                        "ECS stop failed"
                    );
                }
            }
            // Fixture workers check their fenced lease every 10 seconds and exit when revoked.
            "process" => (),
            _ => bail!("unknown executor"),
        }
        Ok(())
    }
}

pub async fn dispatch_one(
    store: &Store,
    executor: &Executor,
    platform: &Platform,
    http: &reqwest::Client,
) -> Result<bool> {
    let mut tx = store.pool.begin().await?;
    let row: Option<(Uuid, serde_json::Value, i32)> = sqlx::query_as("SELECT id,document,tries FROM outbox WHERE status='pending' AND available_at<=now() ORDER BY available_at LIMIT 1 FOR UPDATE SKIP LOCKED").fetch_optional(&mut *tx).await?;
    let Some((id, document, tries)) = row else {
        return Ok(false);
    };
    let effect: Effect = serde_json::from_value(document)?;
    if let Some(attempt) = effect.attempt_id {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,1))")
            .bind(attempt.to_string())
            .execute(&mut *tx)
            .await?;
    }
    let job = store.job(effect.job_id).await?;
    let result = match effect.kind.as_str() {
        "start" => {
            executor
                .start(
                    &job,
                    effect.attempt_id.context("missing attempt")?,
                    platform,
                )
                .await
        }
        "stop" => {
            executor
                .stop(effect.attempt_id.context("missing attempt")?)
                .await
        }
        "notify" => {
            notify(
                store,
                &executor.secret,
                &job,
                effect.gate_id.context("missing gate")?,
                platform,
                http,
            )
            .await
        }
        _ => bail!("unknown outbox effect"),
    };
    match result {
        Ok(()) => {
            sqlx::query(
                "UPDATE outbox SET status='done',tries=tries+1,last_error=NULL WHERE id=$1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        Err(error) => {
            tracing::warn!(effect=%id, kind=%effect.kind, %error, "dispatch will retry");
            sqlx::query("UPDATE outbox SET tries=tries+1,available_at=now()+make_interval(secs=>$2),last_error=$3 WHERE id=$1").bind(id).bind((2_i32.pow((tries as u32).min(5)) * 2) as f64).bind(error.to_string()).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(true)
}
async fn notify(
    store: &Store,
    master: &str,
    job: &Job,
    gate_id: Uuid,
    platform: &Platform,
    http: &reqwest::Client,
) -> Result<()> {
    let gate = job
        .gates
        .iter()
        .find(|g| g.id == gate_id)
        .context("gate missing")?;
    if gate.status != "pending" {
        return Ok(());
    }
    if !job
        .phase()
        .gate
        .as_ref()
        .is_some_and(|g| g.channels.iter().any(|c| c == "slack"))
    {
        return Ok(());
    }
    let channel =
        crate::connectors::slack_channel(store, master, platform, &job.repository.id).await?;
    let token =
        crate::connectors::credential(store, master, "slack", "token", "FACTORY_SLACK_BOT_TOKEN")
            .await?;
    let report_link = std::env::var("FACTORY_PORTAL_URL")
        .ok()
        .and_then(|base| {
            let url = reqwest::Url::parse(&base).ok()?;
            if url.scheme() != "https" || url.host_str().is_none() {
                return None;
            }
            let artifact = job
                .attempts
                .iter()
                .find(|a| a.id == gate.attempt_id)?
                .artifacts
                .values()
                .next()?;
            Some(format!(
                "Report: <{}#report/{}/{}|View {} in portal>\n",
                base.trim_end_matches('/'),
                job.id,
                artifact.id,
                artifact.name.replace(['<', '>', '|'], "")
            ))
        })
        .unwrap_or_default();
    let text = format!("{} · {} is awaiting review.\n{}Job: {}\nArtifact digest: {}\nApprove: /factory approve {} {}\nReject: /factory reject {} {}\nExpires: {}", job.issue.key, gate.phase, report_link, job.id, gate.artifact_digest, gate.id, gate.artifact_digest, gate.id, gate.artifact_digest, gate.deadline);
    let response: serde_json::Value = http
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(token)
        .json(&json!({"channel":channel,"text":text,"client_msg_id":gate_id.to_string()}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(
        response["ok"] == true,
        "Slack rejected approval notification"
    );
    Ok(())
}
