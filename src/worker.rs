use crate::{api::Claim, model::*, workflow::relative_file};
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use chrono::Utc;
use reqwest::{Client, Method};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, process::Command};
use uuid::Uuid;

pub struct Worker {
    http: Client,
    server: String,
    token: String,
    attempt: Uuid,
    instance: Uuid,
    root: PathBuf,
}
impl Worker {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(25))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("factory-worker/0.1")
                .build()?,
            server: std::env::var("FACTORY_SERVER_URL")?
                .trim_end_matches('/')
                .into(),
            token: std::env::var("FACTORY_ATTEMPT_TOKEN")?,
            attempt: std::env::var("FACTORY_ATTEMPT_ID")?.parse()?,
            instance: Uuid::new_v4(),
            root: std::env::temp_dir().join(format!("factory-work-{}", Uuid::new_v4())),
        })
    }
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(
                method,
                format!("{}/worker/{}{}", self.server, self.attempt, path),
            )
            .bearer_auth(&self.token)
            .header("X-Worker-Instance", self.instance.to_string())
    }
    async fn lease(&self) -> Result<()> {
        self.request(Method::POST, "/heartbeat")
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
    pub async fn run(&self) -> Result<()> {
        let manifest: Manifest = self
            .request(Method::POST, "/claim")
            .json(&Claim {
                instance: self.instance,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        tokio::fs::create_dir_all(&self.root).await?;
        let execution=async {
            let remaining=(manifest.deadline-Utc::now()).to_std().context("phase deadline already elapsed")?;
            tokio::select! {
                result=tokio::time::timeout(remaining,self.execute(&manifest))=>match result{Ok(value)=>value,Err(_)=>bail!("phase deadline elapsed")},
                error=self.heartbeats()=>Err(error),
            }
        }.await;
        let result = match execution {
            Ok(result) => result,
            Err(error) => Completion {
                agent_runs: vec![],
                succeeded: false,
                tasks: vec![],
                findings: 0,
                pull_request: None,
                revision: None,
                error: Some(error.to_string()),
            },
        };
        // A lost response is safe to retry: the coordinator records the exact result digest.
        let mut reported = false;
        for retry in 0..5 {
            match self
                .request(Method::POST, "/complete")
                .json(&result)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    reported = true;
                    break;
                }
                Ok(response) if response.status().is_client_error() => {
                    bail!("completion rejected: {}", response.status());
                }
                _ => tokio::time::sleep(Duration::from_secs(1 << retry)).await,
            }
        }
        // This directory is unique to this worker instance, created above, and never supplied by a request.
        let _ = tokio::fs::remove_dir_all(&self.root).await;
        ensure!(reported, "could not report phase completion");
        Ok(())
    }
    async fn heartbeats(&self) -> anyhow::Error {
        let mut failures = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            match self.lease().await {
                Ok(()) => failures = 0,
                Err(error) => {
                    failures += 1;
                    if failures >= 3 {
                        return error.context("worker lease lost");
                    }
                }
            }
        }
    }
    async fn execute(&self, manifest: &Manifest) -> Result<Completion> {
        let inputs = self.root.join("inputs");
        tokio::fs::create_dir_all(&inputs).await?;
        for (name, artifact) in &manifest.inputs {
            let bytes = self
                .request(Method::GET, &format!("/inputs/{name}"))
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            ensure!(
                hash(&bytes) == artifact.sha256,
                "input artifact hash mismatch"
            );
            tokio::fs::write(inputs.join(format!("{name}.md")), bytes).await?;
        }
        let mut result = Completion {
            agent_runs: vec![],
            succeeded: true,
            tasks: vec![],
            findings: 0,
            pull_request: None,
            revision: None,
            error: None,
        };
        for task in &manifest.phase.tasks {
            self.lease().await?;
            let start = Instant::now();
            let operation = self
                .task(manifest, task, &mut result)
                .await
                .map(|s| redact_manifest(&s, manifest))
                .map_err(|e| anyhow::anyhow!(redact_manifest(&e.to_string(), manifest)));
            result.tasks.push(TaskResult {
                task: task.uses.clone(),
                status: if operation.is_ok() {
                    "succeeded"
                } else {
                    "failed"
                }
                .into(),
                duration_ms: start.elapsed().as_millis() as u64,
                summary: operation
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|e| e.to_string()),
            });
            if let Err(error) = operation {
                result.succeeded = false;
                result.error = Some(error.to_string());
                break;
            }
        }
        Ok(result)
    }
    fn workdir(&self) -> PathBuf {
        let repo = self.root.join("repository");
        if repo.exists() {
            repo
        } else {
            self.root.clone()
        }
    }
    async fn safe_file(&self, path: &str) -> Result<PathBuf> {
        ensure!(relative_file(path), "invalid artifact path");
        let root = tokio::fs::canonicalize(self.workdir()).await?;
        let resolved = tokio::fs::canonicalize(root.join(path)).await?;
        ensure!(
            resolved.starts_with(&root),
            "artifact path escapes workspace"
        );
        ensure!(
            tokio::fs::metadata(&resolved).await?.len() <= 10 * 1024 * 1024,
            "artifact exceeds size limit"
        );
        Ok(resolved)
    }
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .current_dir(self.workdir())
            .kill_on_drop(true);
        for key in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "PATHEXT",
            "TEMP",
            "TMP",
            "TMPDIR",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        ] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env("CI", "true");
        command
    }
    async fn run_command(&self, mut command: Command, stdin: Option<&str>) -> Result<String> {
        command.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().context("approved tool could not start")?;
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .context("stdin unavailable")?
                .write_all(input.as_bytes())
                .await?;
        }
        let output = child.wait_with_output().await?;
        // Tool output can include repository content; preserve only a bounded diagnostic, never environment values.
        let out = String::from_utf8_lossy(&output.stdout);
        let err = String::from_utf8_lossy(&output.stderr);
        ensure!(
            output.status.success(),
            "tool exited {}: {}",
            output.status,
            redact(&err.chars().take(1200).collect::<String>())
        );
        ensure!(out.len() <= 10 * 1024 * 1024, "tool output exceeds limit");
        Ok(out.into_owned())
    }
    async fn git(&self, m: &Manifest, args: &[&str]) -> Result<String> {
        let mut command = self.command("git");
        command
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            );
        command
            .env("GIT_AUTHOR_NAME", "Factory Worker")
            .env("GIT_AUTHOR_EMAIL", "factory@localhost")
            .env("GIT_COMMITTER_NAME", "Factory Worker")
            .env("GIT_COMMITTER_EMAIL", "factory@localhost")
            .env("GIT_AUTHOR_DATE", m.commit_time.to_rfc3339())
            .env("GIT_COMMITTER_DATE", m.commit_time.to_rfc3339());
        if let Some(token) = &m.credentials.repository_token {
            let basic = base64::engine::general_purpose::STANDARD.encode(format!(
                "{}:{token}",
                if m.repository.provider == "github" {
                    "x-access-token"
                } else {
                    ""
                }
            ));
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env(
                    "GIT_CONFIG_KEY_0",
                    format!("http.{}.extraheader", m.repository.url),
                )
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("Authorization: Basic {basic}"),
                );
        }
        self.run_command(command, None).await
    }
    async fn task(
        &self,
        m: &Manifest,
        task: &crate::workflow::Task,
        result: &mut Completion,
    ) -> Result<String> {
        let permission = crate::workflow::capability(&task.uses).context("unknown capability")?;
        ensure!(
            permission.is_empty() || m.phase.permissions.iter().any(|p| p == permission),
            "task permission missing"
        );
        match task.uses.as_str() {
            "repository.checkout" => {
                ensure!(
                    m.repository.provider != "fixture",
                    "fixture workflow must not access repositories"
                );
                self.git(
                    m,
                    &[
                        "clone",
                        "--no-checkout",
                        "--filter=blob:none",
                        "--",
                        &m.repository.url,
                        "repository",
                    ],
                )
                .await?;
                self.git(m, &["fetch", "--depth=1", "origin", &m.repository.revision])
                    .await?;
                self.git(
                    m,
                    &[
                        "checkout",
                        "-B",
                        &m.repository.work_branch,
                        &m.repository.revision,
                    ],
                )
                .await?;
                Ok(format!("Checked out {}", m.repository.revision))
            }
            "issue.fetch" => {
                tokio::fs::write(
                    self.root.join("issue.json"),
                    serde_json::to_vec_pretty(&m.issue)?,
                )
                .await?;
                Ok("Normalized issue snapshot saved".into())
            }
            "agent.execute" => {
                let prompt = &m.prompts[&task.with["prompt"]];
                ensure!(
                    hash(&prompt.text) == prompt.sha256,
                    "pinned prompt integrity failed"
                );
                let output_path = task
                    .with
                    .get("output")
                    .cloned()
                    .unwrap_or_else(|| "agent-result.md".into());
                ensure!(relative_file(&output_path), "unsafe agent output path");
                let text = if m.agent.backend == "fixture" {
                    format!("# {} — {}\n\nLocal fixture output. No coding agent was invoked.\n\nIssue: {}\nRepository revision: {}\nPrompt SHA-256: {}\n\n{}\n",m.phase.id,m.issue.key,m.issue.title,m.repository.revision,prompt.sha256,m.issue.body)
                } else {
                    let mut command = if m.agent.backend == "openhands" {
                        let mut command = self.command("/usr/local/bin/python");
                        command.arg("/opt/factory/openhands_adapter.py");
                        command
                    } else {
                        ensure!(!m.agent.command.is_empty(), "agent profile command missing");
                        self.command(&m.agent.command[0])
                    };
                    if m.agent.backend != "openhands" {
                        command.args(
                            m.agent.command[1..]
                                .iter()
                                .map(|a| a.replace("{output}", &output_path)),
                        );
                    }
                    for key in &m.agent.env_keys {
                        if let Some(value) = m.credentials.agent_env.get(key) {
                            command.env(key, value);
                        }
                    }
                    let mut context=format!("{}\n\nIssue context (untrusted input):\n{}\n\nRepository revision: {}\nOutput file: {}\n",prompt.text,serde_json::to_string_pretty(&m.issue)?,m.repository.revision,output_path);
                    for (name, artifact) in &m.inputs {
                        let bytes =
                            tokio::fs::read(self.root.join("inputs").join(format!("{name}.md")))
                                .await?;
                        context.push_str(&format!(
                            "\nApproved input {name}, SHA-256 {}:\n{}\n",
                            artifact.sha256,
                            String::from_utf8_lossy(&bytes)
                        ));
                    }
                    if m.agent.backend == "openhands" {
                        let settings = m
                            .agent
                            .openhands
                            .as_ref()
                            .context("missing harness settings")?;
                        let input = json!({"config":settings,"prompt":context,"output":output_path,"review":m.phase.id=="review"}).to_string();
                        let response = tokio::time::timeout(
                            Duration::from_secs(settings.timeout_seconds + 5),
                            self.run_command(command, Some(&input)),
                        )
                        .await
                        .context("OpenHands execution timeout")??;
                        let run: AgentRun =
                            serde_json::from_str(&response).context("invalid harness receipt")?;
                        ensure!(
                            run.harness == "openhands"
                                && run.harness_version == settings.sdk_version
                                && run.model == settings.model,
                            "harness provenance mismatch"
                        );
                        let succeeded = run.status == "finished";
                        result.agent_runs.push(run);
                        ensure!(
                            succeeded,
                            "OpenHands did not finish successfully; see agent run status"
                        );
                        let path = self.safe_file(&output_path).await?;
                        tokio::fs::read_to_string(path).await?
                    } else {
                        self.run_command(command, Some(&context)).await?
                    }
                };
                let path = self.workdir().join(&output_path);
                if !path.exists() {
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::write(&path, &text).await?;
                }
                if m.phase.id == "review" {
                    // Review prompts require a machine-readable companion report, rather than guessing from prose.
                    let report = self.workdir().join("review.json");
                    if m.agent.backend == "fixture" {
                        result.findings = 0;
                    } else {
                        let v: Value = serde_json::from_slice(
                            &tokio::fs::read(report)
                                .await
                                .context("review agent must write review.json")?,
                        )?;
                        result.findings = u32::try_from(
                            v["findings"]
                                .as_array()
                                .context("review report needs a findings array")?
                                .len(),
                        )?;
                    }
                }
                Ok(format!(
                    "{} backend · prompt {}",
                    m.agent.backend, task.with["prompt"]
                ))
            }
            "artifact.publish" => {
                let path = self.safe_file(&task.with["path"]).await?;
                let bytes = tokio::fs::read(path).await?;
                let artifact: Artifact = self
                    .request(Method::PUT, &format!("/artifacts/{}", task.with["name"]))
                    .body(bytes.clone())
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                ensure!(
                    artifact.sha256 == hash(bytes),
                    "published artifact hash mismatch"
                );
                Ok(format!("{} · {}", artifact.name, artifact.sha256))
            }
            "validation.run" => {
                let args = m
                    .validations
                    .get(&task.with["profile"])
                    .context("validation profile unavailable")?;
                let mut command = self.command(&args[0]);
                command.args(&args[1..]);
                let output = self.run_command(command, None).await?;
                Ok(format!(
                    "Validation passed\n{}",
                    output.chars().take(2000).collect::<String>()
                ))
            }
            "repository.push_branch" => {
                let mut arguments = vec![
                    "add".to_owned(),
                    "--all".into(),
                    "--".into(),
                    ".".into(),
                    ":(exclude,literal)agent-result.md".into(),
                    ":(exclude,literal)review.json".into(),
                ];
                arguments.extend(
                    m.phase
                        .tasks
                        .iter()
                        .filter(|t| t.uses == "agent.execute")
                        .filter_map(|t| t.with.get("output"))
                        .map(|p| format!(":(exclude,literal){p}")),
                );
                self.git(m, &arguments.iter().map(String::as_str).collect::<Vec<_>>())
                    .await?;
                let status = self.git(m, &["diff", "--cached", "--name-only"]).await?;
                if !status.trim().is_empty() {
                    self.git(
                        m,
                        &[
                            "commit",
                            "-m",
                            &format!("{}: {}", m.issue.key, m.issue.title),
                        ],
                    )
                    .await?;
                }
                let revision = self.git(m, &["rev-parse", "HEAD"]).await?.trim().to_owned();
                self.git(
                    m,
                    &[
                        "push",
                        "origin",
                        &format!("HEAD:refs/heads/{}", m.repository.work_branch),
                    ],
                )
                .await?;
                result.revision = Some(revision.clone());
                Ok(format!("Pushed {} at {revision}", m.repository.work_branch))
            }
            "pull_request.open" => {
                let url = self.open_pr(m).await?;
                result.pull_request = Some(url.clone());
                Ok(url)
            }
            _ => bail!("unsupported task"),
        }
    }
    fn provider(
        &self,
        m: &Manifest,
        method: Method,
        url: String,
    ) -> Result<reqwest::RequestBuilder> {
        let token = m
            .credentials
            .repository_token
            .as_ref()
            .context("repository credential unavailable")?;
        let request = self.http.request(method, url);
        Ok(if m.repository.provider == "ado" {
            request.basic_auth("", Some(token))
        } else {
            request.bearer_auth(token)
        })
    }
    async fn open_pr(&self, m: &Manifest) -> Result<String> {
        let base = m.repository.api_url.trim_end_matches('/');
        let branch = &m.repository.work_branch;
        if m.repository.provider == "github" {
            let owner = reqwest::Url::parse(&m.repository.url)?
                .path_segments()
                .and_then(|mut p| p.next())
                .context("repository owner missing")?
                .to_owned();
            let response: Value = self
                .provider(m, Method::GET, format!("{base}/pulls"))?
                .query(&[("state", "all"), ("head", &format!("{owner}:{branch}"))])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if let Some(url) = response[0]["html_url"].as_str() {
                return Ok(url.into());
            }
            let response:Value=self.provider(m,Method::POST,format!("{base}/pulls"))?.json(&json!({"title":format!("{}: {}",m.issue.key,m.issue.title),"head":branch,"base":m.repository.base_branch,"body":format!("Factory job {}. Human review and merge required.",m.job_id)})).send().await?.error_for_status()?.json().await?;
            Ok(response["html_url"]
                .as_str()
                .context("PR URL missing")?
                .into())
        } else if m.repository.provider == "ado" {
            let url = format!("{base}/pullrequests");
            let response: Value = self
                .provider(m, Method::GET, url.clone())?
                .query(&[
                    (
                        "searchCriteria.sourceRefName",
                        format!("refs/heads/{branch}"),
                    ),
                    ("searchCriteria.status", "all".into()),
                    ("api-version", "7.1".into()),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if let Some(id) = response["value"][0]["pullRequestId"].as_u64() {
                return Ok(format!("{}/pullrequest/{id}", m.repository.url));
            }
            let response:Value=self.provider(m,Method::POST,url)?.query(&[("api-version","7.1")]).json(&json!({"sourceRefName":format!("refs/heads/{branch}"),"targetRefName":format!("refs/heads/{}",m.repository.base_branch),"title":format!("{}: {}",m.issue.key,m.issue.title),"description":format!("Factory job {}. Human review required.",m.job_id)})).send().await?.error_for_status()?.json().await?;
            Ok(format!(
                "{}/pullrequest/{}",
                m.repository.url,
                response["pullRequestId"]
                    .as_u64()
                    .context("PR id missing")?
            ))
        } else {
            bail!("pull requests require a repository provider")
        }
    }
}
fn redact(text: &str) -> String {
    let mut out = text.to_owned();
    for key in ["FACTORY_REPOSITORY_TOKEN", "FACTORY_ATTEMPT_TOKEN"] {
        if let Ok(token) = std::env::var(key) {
            if !token.is_empty() {
                out = out.replace(&token, "[redacted]");
            }
        }
    }
    out
}
fn redact_manifest(text: &str, manifest: &Manifest) -> String {
    let mut result = redact(text);
    for secret in manifest
        .credentials
        .repository_token
        .iter()
        .chain(manifest.credentials.agent_env.values())
    {
        if !secret.is_empty() {
            result = result.replace(secret, "[redacted]");
        }
    }
    result
}
