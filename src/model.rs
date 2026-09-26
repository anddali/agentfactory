use crate::{config::Snapshot, workflow::Phase};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

pub fn hash(bytes: impl AsRef<[u8]>) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    pub url: String,
    pub revision: String,
    pub base_branch: String,
    pub work_branch: String,
    pub provider: String,
    pub api_url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub provider: String,
    pub key: String,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<serde_json::Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub workflow: String,
    pub repository: String,
    pub issue: Issue,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub id: Uuid,
    pub key: String,
    pub repository: String,
    pub issue: Issue,
    pub created_at: DateTime<Utc>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    AwaitingApproval,
    Succeeded,
    Failed,
    TimedOut,
    Rejected,
    Cancelled,
}
impl JobStatus {
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::TimedOut | Self::Rejected | Self::Cancelled
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub case_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub root_id: Uuid,
    pub depth: u32,
    pub workflow: String,
    pub snapshot: Snapshot,
    pub repository: Repository,
    pub issue: Issue,
    pub status: JobStatus,
    pub phase_index: usize,
    pub attempts: Vec<Attempt>,
    pub gates: Vec<Gate>,
    #[serde(default)]
    pub upstream_artifacts: BTreeMap<String, Artifact>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub requested_by: String,
    #[serde(default)]
    pub attention_dismissed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub id: Uuid,
    pub phase: String,
    pub number: u32,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub deadline: DateTime<Utc>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub instance: Option<Uuid>,
    pub artifacts: BTreeMap<String, Artifact>,
    pub result: Option<Completion>,
    pub result_hash: Option<String>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: Uuid,
    pub attempt_id: Uuid,
    pub name: String,
    pub sha256: String,
    pub size: usize,
    pub created_at: DateTime<Utc>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gate {
    pub id: Uuid,
    pub attempt_id: Uuid,
    pub phase: String,
    pub artifact_digest: String,
    pub status: String,
    pub deadline: DateTime<Utc>,
    pub decided_by: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_id: Option<String>,
    pub channel: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    #[serde(default)]
    pub agent_runs: Vec<AgentRun>,
    pub succeeded: bool,
    pub tasks: Vec<TaskResult>,
    #[serde(default)]
    pub findings: u32,
    pub pull_request: Option<String>,
    pub revision: Option<String>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRun {
    pub harness: String,
    pub harness_version: String,
    pub model: String,
    pub status: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task: String,
    pub status: String,
    pub duration_ms: u64,
    pub summary: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub event_id: String,
    pub gate_id: Uuid,
    pub artifact_digest: String,
    pub approve: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub job_id: Uuid,
    pub case_id: Uuid,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub actor: String,
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Effect {
    pub id: Uuid,
    pub job_id: Uuid,
    pub attempt_id: Option<Uuid>,
    pub gate_id: Option<Uuid>,
    pub kind: String,
}
#[derive(Debug, Default)]
pub struct Changes {
    pub events: Vec<Event>,
    pub effects: Vec<Effect>,
    pub follow_up: Option<Box<Job>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub job_id: Uuid,
    pub attempt_id: Uuid,
    pub phase: Phase,
    pub repository: Repository,
    pub issue: Issue,
    pub prompts: BTreeMap<String, crate::config::Prompt>,
    pub agent: crate::config::AgentProfile,
    pub validations: BTreeMap<String, Vec<String>>,
    pub inputs: BTreeMap<String, Artifact>,
    pub deadline: DateTime<Utc>,
    pub commit_time: DateTime<Utc>,
    #[serde(default)]
    pub credentials: WorkerCredentials,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct WorkerCredentials {
    pub repository_token: Option<String>,
    pub agent_env: BTreeMap<String, String>,
}
impl std::fmt::Debug for WorkerCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkerCredentials([redacted])")
    }
}
