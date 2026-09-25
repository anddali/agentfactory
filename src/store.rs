use crate::{config::Snapshot, model::*};
use anyhow::{ensure, Context, Result};
use chrono::Utc;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    pub pool: PgPool,
}
pub type Tx<'a> = Transaction<'a, Postgres>;
impl Store {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(12)
            .connect(url)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }
    pub async fn job(&self, id: Uuid) -> Result<Job> {
        let value: serde_json::Value = sqlx::query_scalar("SELECT document FROM jobs WHERE id=$1")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(serde_json::from_value(value)?)
    }
    pub async fn attempt_job(&self, id: Uuid) -> Result<Job> {
        let value: serde_json::Value =
            sqlx::query_scalar("SELECT document FROM jobs WHERE document->'attempts' @> $1::jsonb")
                .bind(serde_json::json!([{"id":id}]))
                .fetch_one(&self.pool)
                .await?;
        Ok(serde_json::from_value(value)?)
    }
    pub async fn locked(tx: &mut Tx<'_>, id: Uuid) -> Result<Job> {
        let value: serde_json::Value =
            sqlx::query_scalar("SELECT document FROM jobs WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        Ok(serde_json::from_value(value)?)
    }
    pub async fn persist(tx: &mut Tx<'_>, job: &Job, changes: Changes) -> Result<()> {
        Self::save_job(tx, job).await?;
        if let Some(child) = changes.follow_up {
            Self::save_job(tx, &child).await?;
        }
        for event in changes.events {
            sqlx::query("INSERT INTO events(id,job_id,document) VALUES($1,$2,$3)")
                .bind(event.id)
                .bind(event.job_id)
                .bind(serde_json::to_value(event)?)
                .execute(&mut **tx)
                .await?;
        }
        for effect in changes.effects {
            sqlx::query("INSERT INTO outbox(id,job_id,document) VALUES($1,$2,$3)")
                .bind(effect.id)
                .bind(effect.job_id)
                .bind(serde_json::to_value(effect)?)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }
    async fn save_job(tx: &mut Tx<'_>, job: &Job) -> Result<()> {
        let status = serde_json::to_value(&job.status)?
            .as_str()
            .unwrap()
            .to_owned();
        sqlx::query("INSERT INTO jobs(id,case_id,status,document,created_at) VALUES($1,$2,$3,$4,$5) ON CONFLICT(id) DO UPDATE SET status=$3,document=$4,updated_at=now()")
            .bind(job.id).bind(job.case_id).bind(status).bind(serde_json::to_value(job)?).bind(job.created_at).execute(&mut **tx).await?;
        Ok(())
    }
    pub async fn existing(&self, key: &str, digest: &str) -> Result<Option<Job>> {
        let row: Option<(String, Uuid)> =
            sqlx::query_as("SELECT body_hash,job_id FROM requests WHERE request_key=$1")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        if let Some((old, id)) = row {
            ensure!(
                old == digest,
                "idempotency key reused with different request"
            );
            return Ok(Some(self.job(id).await?));
        }
        Ok(None)
    }
    pub async fn submit(
        &self,
        request_key: &str,
        digest: &str,
        submission: Submission,
        mut repository: Repository,
        snapshot: Snapshot,
        actor: &str,
    ) -> Result<Job> {
        let mut tx = self.pool.begin().await?;
        // Serialize only matching external requests. Unique keys remain the final invariant.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(request_key)
            .execute(&mut *tx)
            .await?;
        let existing: Option<(String, Uuid)> =
            sqlx::query_as("SELECT body_hash,job_id FROM requests WHERE request_key=$1")
                .bind(request_key)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((old, id)) = existing {
            ensure!(
                old == digest,
                "idempotency key reused with different request"
            );
            let job = Self::locked(&mut tx, id).await?;
            tx.commit().await?;
            return Ok(job);
        }
        ensure!(
            snapshot.workflows.contains_key(&submission.workflow),
            "unknown workflow"
        );
        let now = Utc::now();
        let id = Uuid::new_v4();
        let case_id = Uuid::new_v4();
        let key = format!(
            "{}:{}:{}",
            submission.repository, submission.issue.provider, submission.issue.key
        );
        let case = Case {
            id: case_id,
            key: key.clone(),
            repository: submission.repository,
            issue: submission.issue.clone(),
            created_at: now,
        };
        let actual_id: Uuid = sqlx::query_scalar("INSERT INTO cases(id,external_key,document) VALUES($1,$2,$3) ON CONFLICT(external_key) DO UPDATE SET external_key=EXCLUDED.external_key RETURNING id").bind(case_id).bind(key).bind(serde_json::to_value(case)?).fetch_one(&mut *tx).await?;
        if repository.work_branch.is_empty() {
            repository.work_branch = format!("factory/{id}");
        }
        let mut job = Job {
            id,
            root_id: id,
            case_id: actual_id,
            parent_id: None,
            depth: 0,
            workflow: submission.workflow,
            snapshot,
            repository,
            issue: submission.issue,
            status: JobStatus::Queued,
            phase_index: 0,
            attempts: vec![],
            gates: vec![],
            upstream_artifacts: Default::default(),
            created_at: now,
            finished_at: None,
            requested_by: actor.into(),
            attention_dismissed: false,
        };
        let mut changes = Changes::default();
        changes.event(
            &job,
            "job_created",
            actor,
            "Workflow and repository revision pinned",
            now,
        );
        job.schedule(now, &mut changes);
        Self::persist(&mut tx, &job, changes).await?;
        sqlx::query("INSERT INTO requests(request_key,body_hash,job_id) VALUES($1,$2,$3)")
            .bind(request_key)
            .bind(digest)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(job)
    }
    pub async fn mutate<F>(&self, id: Uuid, f: F) -> Result<Job>
    where
        F: FnOnce(&mut Job, &mut Changes) -> Result<()>,
    {
        let mut tx = self.pool.begin().await?;
        let mut job = Self::locked(&mut tx, id).await?;
        let mut changes = Changes::default();
        f(&mut job, &mut changes)?;
        Self::persist(&mut tx, &job, changes).await?;
        tx.commit().await?;
        Ok(job)
    }
    pub async fn reconcile(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<serde_json::Value> = sqlx::query_scalar("SELECT document FROM jobs WHERE status IN ('queued','running','awaiting_approval') ORDER BY updated_at LIMIT 100 FOR UPDATE SKIP LOCKED").fetch_all(&mut *tx).await?;
        for row in rows {
            let mut job: Job = serde_json::from_value(row)?;
            let mut changes = Changes::default();
            job.reconcile(Utc::now(), &mut changes)?;
            Self::persist(&mut tx, &job, changes).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn jobs(&self) -> Result<Vec<Job>> {
        let rows: Vec<serde_json::Value> =
            sqlx::query_scalar("SELECT document FROM jobs ORDER BY created_at DESC LIMIT 500")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|r| serde_json::from_value(r).context("stored job could not be decoded"))
            .collect()
    }
    pub async fn events(&self, job_id: Option<Uuid>) -> Result<Vec<Event>> {
        let rows: Vec<serde_json::Value> = sqlx::query_scalar("SELECT document FROM events WHERE ($1::uuid IS NULL OR job_id=$1) ORDER BY sequence DESC LIMIT 500").bind(job_id).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|r| serde_json::from_value(r).context("stored event could not be decoded"))
            .collect()
    }
}
