use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workflow {
    pub api_version: String,
    pub kind: String,
    pub id: String,
    pub version: u32,
    pub description: String,
    pub inputs: BTreeMap<String, Input>,
    pub defaults: Defaults,
    pub phases: Vec<Phase>,
    #[serde(default)]
    pub follow_ups: Vec<FollowUp>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub r#type: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Defaults {
    pub worker_profile: String,
    pub agent_profile: String,
    pub repository_revision: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Phase {
    pub id: String,
    #[serde(default)]
    pub agent_profile: Option<String>,
    pub timeout: String,
    #[serde(default = "one")]
    pub max_attempts: u32,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    pub tasks: Vec<Task>,
    pub gate: Option<GateDefinition>,
}
fn one() -> u32 {
    1
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub uses: String,
    #[serde(default)]
    pub with: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GateDefinition {
    pub channels: Vec<String>,
    pub approvers: String,
    pub timeout: String,
    pub on_reject: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FollowUp {
    pub workflow: String,
    pub when: String,
    pub max_depth: u32,
}

pub fn seconds(value: &str) -> Result<i64> {
    let (digits, suffix) = value
        .split_at_checked(value.len().checked_sub(1).context("empty duration")?)
        .context("duration unit must be ASCII")?;
    let n: i64 = digits
        .parse()
        .context("duration must be a positive integer followed by s, m, h or d")?;
    let multiplier = match suffix {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => bail!("invalid duration unit"),
    };
    ensure!(
        n > 0 && n <= 2_592_000 / multiplier,
        "duration outside 1 second to 30 days"
    );
    Ok(n * multiplier)
}
pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
}
pub fn relative_file(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains(['\\', ':'])
        && value.split('/').all(|p| !matches!(p, "" | "." | ".."))
}
pub fn capability(name: &str) -> Option<&'static str> {
    match name {
        "repository.checkout" => Some("repository.read"),
        "issue.fetch" => Some("issue.read"),
        "agent.execute" | "artifact.publish" | "validation.run" => Some(""),
        "repository.push_branch" => Some("repository.branch.write"),
        "pull_request.open" => Some("pull_request.create"),
        _ => None,
    }
}
impl Workflow {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.api_version == "factory/v1" && self.kind == "Workflow",
            "unsupported workflow schema"
        );
        ensure!(
            identifier(&self.id) && self.version > 0,
            "invalid workflow identity"
        );
        ensure!(
            self.defaults.repository_revision == "pin_at_job_start",
            "only pinned repository revisions are supported"
        );
        ensure!(
            self.inputs
                .get("repository")
                .is_some_and(|i| i.r#type == "repository_ref"),
            "repository_ref input required"
        );
        for (key, input) in &self.inputs {
            ensure!(
                matches!(
                    (key.as_str(), input.r#type.as_str()),
                    ("repository", "repository_ref") | ("issue", "issue_ref")
                ),
                "unsupported input {key}"
            );
        }
        ensure!(
            !self.phases.is_empty() && self.phases.len() <= 20,
            "expected 1–20 sequential phases"
        );
        let mut phases = BTreeSet::new();
        let mut outputs = BTreeSet::new();
        for phase in &self.phases {
            ensure!(
                identifier(&phase.id) && phases.insert(&phase.id),
                "invalid or duplicate phase id"
            );
            seconds(&phase.timeout)?;
            ensure!(
                (1..=5).contains(&phase.max_attempts),
                "phase maxAttempts must be 1–5"
            );
            ensure!(
                !phase.tasks.is_empty() && phase.tasks.len() <= 30,
                "expected 1–30 tasks"
            );
            for (name, input) in &phase.inputs {
                let parent = input
                    .strip_prefix("parent.artifacts.")
                    .is_some_and(identifier);
                ensure!(
                    identifier(name) && (outputs.contains(input) || parent),
                    "phase input {input} must reference an earlier phase or parent job artifact"
                );
            }
            let mut phase_outputs = BTreeSet::new();
            let mut produced = BTreeSet::new();
            for task in &phase.tasks {
                let permission = capability(&task.uses)
                    .with_context(|| format!("unknown task {}", task.uses))?;
                ensure!(
                    permission.is_empty() || phase.permissions.iter().any(|p| p == permission),
                    "{} requires {permission}",
                    task.uses
                );
                let allowed: &[&str] = match task.uses.as_str() {
                    "agent.execute" => &["prompt", "output"],
                    "artifact.publish" => &["name", "path"],
                    "validation.run" => &["profile"],
                    _ => &[],
                };
                ensure!(
                    task.with.keys().all(|k| allowed.contains(&k.as_str())),
                    "unsupported task argument"
                );
                if task.uses == "agent.execute" {
                    ensure!(
                        task.with.get("prompt").is_some_and(|p| p.contains('@')),
                        "versioned prompt required"
                    );
                    if let Some(path) = task.with.get("output") {
                        ensure!(relative_file(path), "unsafe output path");
                        produced.insert(path.clone());
                    }
                }
                if task.uses == "artifact.publish" {
                    let name = task.with.get("name").context("artifact name required")?;
                    let path = task.with.get("path").context("artifact path required")?;
                    ensure!(
                        identifier(name) && relative_file(path),
                        "invalid artifact name/path"
                    );
                    ensure!(
                        produced.contains(path),
                        "artifact must be produced by an earlier agent task"
                    );
                    ensure!(
                        phase_outputs.insert(format!("phases.{}.artifacts.{name}", phase.id)),
                        "duplicate artifact name"
                    );
                }
                if task.uses == "validation.run" {
                    ensure!(
                        task.with.contains_key("profile"),
                        "validation profile required"
                    );
                }
            }
            if let Some(gate) = &phase.gate {
                seconds(&gate.timeout)?;
                ensure!(
                    !phase_outputs.is_empty(),
                    "gated phases must publish an artifact"
                );
                ensure!(
                    !gate.channels.is_empty()
                        && gate
                            .channels
                            .iter()
                            .all(|c| matches!(c.as_str(), "slack" | "api")),
                    "unsupported gate channel"
                );
                ensure!(
                    gate.approvers == "repository_maintainers" && gate.on_reject == "stop",
                    "unsupported gate policy"
                );
            }
            outputs.extend(phase_outputs);
        }
        ensure!(
            self.follow_ups.len() <= 1,
            "only one conditional follow-up per workflow is supported"
        );
        for next in &self.follow_ups {
            ensure!(
                (1..=10).contains(&next.max_depth)
                    && matches!(next.when.as_str(), "always" | "findings"),
                "unbounded or invalid follow-up"
            );
        }
        Ok(())
    }
}
