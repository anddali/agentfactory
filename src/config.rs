use crate::{
    model::hash,
    workflow::{identifier, Workflow},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub policy_version: String,
    pub allow_fixture: bool,
    pub allowed_permissions: Vec<String>,
    pub workers: BTreeMap<String, WorkerProfile>,
    pub agents: BTreeMap<String, AgentProfile>,
    pub validations: BTreeMap<String, Vec<String>>,
    /// Optional aliases and per-repository overrides, never an access allowlist.
    #[serde(default)]
    pub repositories: BTreeMap<String, RepositoryConfig>,
    #[serde(default = "default_workflow")]
    pub default_workflow: String,
}
fn default_workflow() -> String {
    "research-plan".into()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerProfile {
    pub version: u32,
    pub image: String,
    pub cpus: String,
    pub memory: String,
    #[serde(default)]
    pub ecs_task_definition: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    pub version: u32,
    pub backend: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub env_keys: Vec<String>,
    #[serde(default)]
    pub openhands: Option<OpenHandsConfig>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenHandsConfig {
    pub sdk_version: String,
    /// LiteLLM provider/model identifier; pinned in the job snapshot.
    pub model: String,
    pub api_key_env: String,
    pub base_url: Option<String>,
    #[serde(default = "auto_api")]
    pub api_mode: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub max_iterations: u32,
    pub max_output_tokens: u32,
    pub timeout_seconds: u64,
    pub tools: Vec<String>,
}
fn auto_api() -> String {
    "auto".into()
}
impl OpenHandsConfig {
    pub fn validate(&self, env_keys: &[String]) -> Result<()> {
        ensure!(
            self.sdk_version == "1.49.2",
            "unsupported OpenHands SDK version"
        );
        ensure!(
            matches!(self.api_mode.as_str(), "auto" | "chat" | "responses"),
            "unsupported model API mode"
        );
        ensure!(
            self.model.contains('/') && !self.model.chars().any(char::is_whitespace),
            "model must be provider/model"
        );
        ensure!(
            env_keys.contains(&self.api_key_env),
            "OpenHands API key must be in env_keys"
        );
        ensure!(
            (1..=200).contains(&self.max_iterations),
            "invalid OpenHands iteration limit"
        );
        ensure!(
            (1..=32768).contains(&self.max_output_tokens),
            "invalid output token limit"
        );
        ensure!(
            (1..=7200).contains(&self.timeout_seconds),
            "invalid OpenHands timeout"
        );
        ensure!(
            !self.tools.is_empty()
                && self
                    .tools
                    .iter()
                    .all(|t| matches!(t.as_str(), "terminal" | "file_editor")),
            "unsupported OpenHands tool"
        );
        if let Some(base) = &self.base_url {
            let url = reqwest::Url::parse(base)?;
            ensure!(
                matches!(url.scheme(), "https" | "http")
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "endpoint must be HTTP(S) without credentials, query or fragment"
            );
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryConfig {
    pub provider: String,
    pub url: String,
    pub api_url: String,
    pub branch: String,
    pub revision: Option<String>,
    pub workflow: String,
    pub maintainers: Vec<String>,
    pub read_token_env: Option<String>,
    pub write_token_env: Option<String>,
    pub slack_channel: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub text: String,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub workflows: BTreeMap<String, Workflow>,
    pub prompts: BTreeMap<String, Prompt>,
    pub workers: BTreeMap<String, WorkerProfile>,
    pub agents: BTreeMap<String, AgentProfile>,
    pub validations: BTreeMap<String, Vec<String>>,
    pub policy_version: String,
    pub platform_version: String,
    #[serde(default)]
    pub platform_revision: String,
    pub definition_hash: String,
}
impl Snapshot {
    pub fn refresh_hash(&mut self) -> Result<()> {
        self.definition_hash.clear();
        self.definition_hash = hash(serde_json::to_vec(self)?);
        Ok(())
    }
}
impl Platform {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_yaml::from_str(&std::fs::read_to_string(path)?)?)
    }
    pub fn snapshot(&self, directory: &Path, prompts: &Path) -> Result<Snapshot> {
        let mut workflows = BTreeMap::new();
        let mut resolved_prompts = BTreeMap::new();
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "yaml") {
                continue;
            }
            let w: Workflow = serde_yaml::from_str(&std::fs::read_to_string(path)?)?;
            w.validate()?;
            ensure!(
                self.workers.contains_key(&w.defaults.worker_profile),
                "unknown worker profile"
            );
            for agent_id in std::iter::once(&w.defaults.agent_profile)
                .chain(w.phases.iter().filter_map(|p| p.agent_profile.as_ref()))
            {
                let agent = self.agents.get(agent_id).context("unknown agent profile")?;
                ensure!(
                    matches!(agent.backend.as_str(), "command" | "fixture" | "openhands"),
                    "unsupported agent backend"
                );
                ensure!(
                    agent.backend != "fixture" || self.allow_fixture,
                    "fixture agent disabled by policy"
                );
                ensure!(
                    agent.backend != "command" || !agent.command.is_empty(),
                    "agent command required"
                );
                if agent.backend == "openhands" {
                    ensure!(
                        agent.command.is_empty(),
                        "OpenHands uses the bundled adapter"
                    );
                    agent
                        .openhands
                        .as_ref()
                        .context("OpenHands configuration required")?
                        .validate(&agent.env_keys)?;
                } else {
                    ensure!(
                        agent.openhands.is_none(),
                        "OpenHands settings require openhands backend"
                    );
                }
                ensure!(
                    agent.env_keys.iter().all(|k| !k.starts_with("FACTORY_")
                        && !k.starts_with("AWS_")
                        && !k.contains('=')),
                    "agent environment cannot include platform credentials"
                );
            }
            for phase in &w.phases {
                for permission in &phase.permissions {
                    ensure!(
                        self.allowed_permissions.contains(permission),
                        "permission rejected by platform policy: {permission}"
                    );
                }
                for task in &phase.tasks {
                    if let Some(name) = task.with.get("prompt") {
                        ensure!(
                            name.split('@').count() == 2 && name.split('@').all(identifier),
                            "invalid prompt reference"
                        );
                        let text = std::fs::read_to_string(prompts.join(format!("{name}.md")))
                            .with_context(|| format!("missing prompt {name}"))?;
                        resolved_prompts.insert(
                            name.clone(),
                            Prompt {
                                sha256: hash(&text),
                                text,
                            },
                        );
                    }
                    if task.uses == "validation.run" {
                        ensure!(
                            self.validations
                                .get(&task.with["profile"])
                                .is_some_and(|c| !c.is_empty()),
                            "unknown validation profile"
                        );
                    }
                }
            }
            ensure!(!workflows.contains_key(&w.id), "duplicate workflow id");
            workflows.insert(w.id.clone(), w);
        }
        ensure!(!workflows.is_empty(), "no workflow definitions found");
        for w in workflows.values() {
            for next in &w.follow_ups {
                ensure!(
                    workflows.contains_key(&next.workflow),
                    "unknown follow-up workflow"
                );
                let produced: Vec<_> = w
                    .phases
                    .last()
                    .unwrap()
                    .tasks
                    .iter()
                    .filter(|t| t.uses == "artifact.publish")
                    .map(|t| t.with["name"].as_str())
                    .collect();
                for source in workflows[&next.workflow]
                    .phases
                    .iter()
                    .flat_map(|p| p.inputs.values())
                    .filter_map(|v| v.strip_prefix("parent.artifacts."))
                {
                    ensure!(
                        produced.contains(&source),
                        "follow-up {source} input is not published by the parent workflow"
                    );
                }
            }
        }
        for (id, repo) in &self.repositories {
            ensure!(
                identifier(id) && workflows.contains_key(&repo.workflow),
                "invalid repository registration"
            );
            ensure!(
                !repo.maintainers.is_empty(),
                "repository must have maintainers"
            );
            ensure!(
                matches!(repo.provider.as_str(), "github" | "ado" | "fixture"),
                "unsupported repository provider"
            );
            ensure!(
                repo.provider != "fixture" || self.allow_fixture,
                "fixture repository disabled"
            );
            if repo.provider != "fixture" {
                let url = reqwest::Url::parse(&repo.url)?;
                ensure!(
                    url.scheme() == "https"
                        && url.username().is_empty()
                        && url.password().is_none(),
                    "repository URL must be HTTPS without credentials"
                );
                ensure!(
                    reqwest::Url::parse(&repo.api_url)?.scheme() == "https",
                    "API URL must use HTTPS"
                );
            }
        }
        let mut snapshot = Snapshot {
            definition_hash: String::new(),
            workflows,
            prompts: resolved_prompts,
            workers: self.workers.clone(),
            agents: self.agents.clone(),
            validations: self.validations.clone(),
            policy_version: self.policy_version.clone(),
            platform_version: env!("CARGO_PKG_VERSION").into(),
            platform_revision: option_env!("FACTORY_BUILD_REVISION")
                .unwrap_or("unversioned")
                .into(),
        };
        snapshot.refresh_hash()?;
        Ok(snapshot)
    }
}
