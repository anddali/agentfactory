//! Pure state transitions. The store commits the returned state, events and dispatches together.
use crate::{model::*, workflow::seconds};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

impl Changes {
    pub fn event(
        &mut self,
        job: &Job,
        kind: &str,
        actor: &str,
        message: impl Into<String>,
        now: DateTime<Utc>,
    ) {
        self.events.push(Event {
            id: Uuid::new_v4(),
            job_id: job.id,
            case_id: job.case_id,
            at: now,
            kind: kind.into(),
            actor: actor.into(),
            message: message.into(),
        });
    }
    pub fn effect(&mut self, job: &Job, kind: &str, attempt: Option<Uuid>, gate: Option<Uuid>) {
        self.effects.push(Effect {
            id: Uuid::new_v4(),
            job_id: job.id,
            attempt_id: attempt,
            gate_id: gate,
            kind: kind.into(),
        });
    }
}
impl Job {
    pub fn phase(&self) -> &crate::workflow::Phase {
        &self.snapshot.workflows[&self.workflow].phases[self.phase_index]
    }
    pub fn current(&self) -> &Attempt {
        self.attempts.last().expect("job always has an attempt")
    }
    pub fn current_mut(&mut self) -> &mut Attempt {
        self.attempts.last_mut().expect("job always has an attempt")
    }
    pub fn schedule(&mut self, now: DateTime<Utc>, changes: &mut Changes) {
        let phase = self.phase().id.clone();
        let number = self.attempts.iter().filter(|a| a.phase == phase).count() as u32 + 1;
        let id = Uuid::new_v4();
        self.attempts.push(Attempt {
            id,
            phase,
            number,
            status: "queued".into(),
            created_at: now,
            started_at: None,
            finished_at: None,
            deadline: now + Duration::seconds(120),
            heartbeat_at: None,
            instance: None,
            artifacts: Default::default(),
            result: None,
            result_hash: None,
            error: None,
        });
        self.status = JobStatus::Queued;
        changes.event(
            self,
            "attempt_queued",
            "coordinator",
            format!("{} · attempt {number}", self.phase().id),
            now,
        );
        changes.effect(self, "start", Some(id), None);
    }
    pub fn active(&self, attempt: Uuid, now: DateTime<Utc>) -> Result<()> {
        ensure!(
            !self.status.terminal()
                && matches!(self.status, JobStatus::Queued | JobStatus::Running),
            "job is not executing"
        );
        ensure!(self.current().id == attempt, "superseded attempt");
        ensure!(now < self.current().deadline, "attempt deadline elapsed");
        Ok(())
    }
    pub fn worker(&self, attempt: Uuid, instance: Uuid, now: DateTime<Utc>) -> Result<()> {
        self.active(attempt, now)?;
        ensure!(
            self.current().instance == Some(instance),
            "worker instance does not own this attempt"
        );
        Ok(())
    }
    pub fn claim(
        &mut self,
        attempt: Uuid,
        instance: Uuid,
        now: DateTime<Utc>,
        changes: &mut Changes,
    ) -> Result<()> {
        self.active(attempt, now)?;
        if self.current().instance.is_some() {
            return self.worker(attempt, instance, now);
        }
        let timeout = seconds(&self.phase().timeout)?;
        let a = self.current_mut();
        a.instance = Some(instance);
        a.started_at = Some(now);
        a.heartbeat_at = Some(now);
        a.deadline = now + Duration::seconds(timeout);
        a.status = "running".into();
        self.status = JobStatus::Running;
        changes.event(
            self,
            "attempt_started",
            "worker",
            self.phase().id.clone(),
            now,
        );
        Ok(())
    }
    pub fn complete(
        &mut self,
        attempt: Uuid,
        instance: Uuid,
        result: Completion,
        now: DateTime<Utc>,
        changes: &mut Changes,
    ) -> Result<()> {
        let digest = hash(serde_json::to_vec(&result)?);
        if let Some(old) = self.attempts.iter().find(|a| a.id == attempt) {
            if old.result_hash.as_deref() == Some(&digest) && old.instance == Some(instance) {
                return Ok(());
            }
        }
        self.worker(attempt, instance, now)?;
        let expected = &self.phase().tasks;
        ensure!(
            result.tasks.len() <= expected.len(),
            "unexpected task results"
        );
        for (i, task) in result.tasks.iter().enumerate() {
            ensure!(
                task.task == expected[i].uses
                    && matches!(task.status.as_str(), "succeeded" | "failed"),
                "invalid ordered task report"
            );
        }
        if result.succeeded {
            let workflow = &self.snapshot.workflows[&self.workflow];
            let agent = &self.snapshot.agents[self
                .phase()
                .agent_profile
                .as_ref()
                .unwrap_or(&workflow.defaults.agent_profile)];
            if agent.backend == "openhands" {
                let config = agent
                    .openhands
                    .as_ref()
                    .context("missing pinned harness configuration")?;
                ensure!(
                    result.agent_runs.len()
                        == expected
                            .iter()
                            .filter(|t| t.uses == "agent.execute")
                            .count()
                        && result.agent_runs.iter().all(|r| r.harness == "openhands"
                            && r.harness_version == config.sdk_version
                            && r.model == config.model
                            && r.status == "finished"),
                    "missing or mismatched harness provenance"
                );
            }
            ensure!(
                result.tasks.len() == expected.len()
                    && result.tasks.iter().all(|t| t.status == "succeeded"),
                "incomplete successful phase report"
            );
            for task in expected.iter().filter(|t| t.uses == "artifact.publish") {
                ensure!(
                    self.current().artifacts.contains_key(&task.with["name"]),
                    "missing published artifact"
                );
            }
        }
        if let Some(revision) = &result.revision {
            ensure!(
                revision.len() == 40 && revision.bytes().all(|c| c.is_ascii_hexdigit()),
                "invalid result revision"
            );
        }
        let a = self.current_mut();
        a.finished_at = Some(now);
        a.result = Some(result.clone());
        a.result_hash = Some(digest);
        a.status = if result.succeeded {
            "succeeded"
        } else {
            "failed"
        }
        .into();
        changes.effect(self, "stop", Some(attempt), None);
        if !result.succeeded {
            return self.fail(
                result.error.as_deref().unwrap_or("phase failed"),
                false,
                now,
                changes,
            );
        }
        changes.event(
            self,
            "phase_completed",
            "worker",
            self.phase().id.clone(),
            now,
        );
        if let Some(definition) = &self.phase().gate {
            let gate = Gate {
                id: Uuid::new_v4(),
                attempt_id: attempt,
                phase: self.phase().id.clone(),
                artifact_digest: hash(serde_json::to_vec(
                    &self
                        .current()
                        .artifacts
                        .iter()
                        .map(|(n, a)| (n, &a.sha256))
                        .collect::<Vec<_>>(),
                )?),
                status: "pending".into(),
                deadline: now + Duration::seconds(seconds(&definition.timeout)?),
                decided_by: None,
                decided_at: None,
                decision_id: None,
                channel: None,
            };
            let id = gate.id;
            self.gates.push(gate);
            self.status = JobStatus::AwaitingApproval;
            changes.event(
                self,
                "approval_requested",
                "coordinator",
                format!("{} · awaiting a maintainer", self.phase().id),
                now,
            );
            changes.effect(self, "notify", Some(attempt), Some(id));
        } else {
            self.advance(now, changes)?;
        }
        Ok(())
    }
    pub fn advance(&mut self, now: DateTime<Utc>, changes: &mut Changes) -> Result<()> {
        if self.phase_index + 1 < self.snapshot.workflows[&self.workflow].phases.len() {
            self.phase_index += 1;
            self.schedule(now, changes);
        } else {
            self.status = JobStatus::Succeeded;
            self.finished_at = Some(now);
            if self.snapshot.workflows[&self.workflow]
                .follow_ups
                .first()
                .is_some_and(|next| {
                    next.when == "findings"
                        && self.depth >= next.max_depth
                        && self
                            .current()
                            .result
                            .as_ref()
                            .is_some_and(|r| r.findings > 0)
                })
            {
                self.status = JobStatus::Failed;
                changes.event(
                    self,
                    "follow_up_exhausted",
                    "coordinator",
                    "Review findings remain after the permitted follow-up cycles",
                    now,
                );
                return Ok(());
            }
            changes.event(
                self,
                "job_succeeded",
                "coordinator",
                "All phases completed",
                now,
            );
            if let Some(next) = self.snapshot.workflows[&self.workflow].follow_ups.first() {
                let findings = self.current().result.as_ref().map_or(0, |r| r.findings);
                if self.depth < next.max_depth && (next.when == "always" || findings > 0) {
                    let mut child = self.clone();
                    child.id = Uuid::new_v4();
                    child.parent_id = Some(self.id);
                    child.depth += 1;
                    child.workflow = next.workflow.clone();
                    child.upstream_artifacts = self.current().artifacts.clone();
                    child.phase_index = 0;
                    child.attempts.clear();
                    child.gates.clear();
                    child.created_at = now;
                    child.finished_at = None;
                    if let Some(revision) = self
                        .current()
                        .result
                        .as_ref()
                        .and_then(|r| r.revision.as_ref())
                    {
                        child.repository.revision = revision.clone();
                    }
                    child.schedule(now, changes);
                    changes.follow_up = Some(Box::new(child));
                }
            }
        }
        Ok(())
    }
    pub fn decide(
        &mut self,
        decision: &Decision,
        actor: &str,
        channel: &str,
        now: DateTime<Utc>,
        changes: &mut Changes,
    ) -> Result<()> {
        let gate = self
            .gates
            .iter()
            .find(|g| g.id == decision.gate_id)
            .context("unknown gate")?;
        ensure!(
            gate.artifact_digest == decision.artifact_digest,
            "artifact version does not match approval"
        );
        if gate.decision_id.as_deref() == Some(&decision.event_id) {
            ensure!(
                gate.decided_by.as_deref() == Some(actor)
                    && gate.channel.as_deref() == Some(channel)
                    && (gate.status == "approved") == decision.approve,
                "decision id reused with different content"
            );
            return Ok(());
        }
        ensure!(
            self.status == JobStatus::AwaitingApproval
                && gate.status == "pending"
                && gate.attempt_id == self.current().id,
            "gate is no longer current"
        );
        ensure!(now < gate.deadline, "gate deadline elapsed");
        ensure!(
            self.phase()
                .gate
                .as_ref()
                .is_some_and(|g| g.channels.iter().any(|c| c == channel)),
            "decision channel not permitted"
        );
        let gate = self
            .gates
            .iter_mut()
            .find(|g| g.id == decision.gate_id)
            .unwrap();
        gate.status = if decision.approve {
            "approved"
        } else {
            "rejected"
        }
        .into();
        gate.decided_by = Some(actor.into());
        gate.decided_at = Some(now);
        gate.decision_id = Some(decision.event_id.clone());
        gate.channel = Some(channel.into());
        changes.event(
            self,
            "approval_decided",
            actor,
            if decision.approve {
                "Approved exact artifact version"
            } else {
                "Rejected"
            },
            now,
        );
        if decision.approve {
            self.advance(now, changes)?;
        } else {
            self.status = JobStatus::Rejected;
            self.finished_at = Some(now);
        }
        Ok(())
    }
    pub fn fail(
        &mut self,
        reason: &str,
        timeout: bool,
        now: DateTime<Utc>,
        changes: &mut Changes,
    ) -> Result<()> {
        let a = self.current_mut();
        a.status = if timeout { "timed_out" } else { "failed" }.into();
        a.error = Some(reason.into());
        a.finished_at = Some(now);
        changes.effect(self, "stop", Some(self.current().id), None);
        changes.event(self, "attempt_failed", "coordinator", reason, now);
        if self.current().number < self.phase().max_attempts {
            self.schedule(now, changes);
        } else {
            self.status = if timeout {
                JobStatus::TimedOut
            } else {
                JobStatus::Failed
            };
            self.finished_at = Some(now);
        }
        Ok(())
    }
    pub fn reconcile(&mut self, now: DateTime<Utc>, changes: &mut Changes) -> Result<()> {
        if matches!(self.status, JobStatus::Queued | JobStatus::Running) {
            let a = self.current();
            if now >= a.deadline {
                let reason = if a.started_at.is_none() {
                    "Worker launch timed out: no worker claimed the attempt before the launch deadline"
                } else {
                    "Phase execution timed out: the worker exceeded the phase deadline"
                };
                self.fail(reason, true, now, changes)?;
            } else if a
                .heartbeat_at
                .is_some_and(|t| now - t > Duration::seconds(90))
            {
                self.fail("Worker heartbeat expired", false, now, changes)?;
            }
        } else if self.status == JobStatus::AwaitingApproval
            && self.gates.last().is_some_and(|g| now >= g.deadline)
        {
            self.gates.last_mut().unwrap().status = "expired".into();
            self.status = JobStatus::TimedOut;
            self.finished_at = Some(now);
            changes.event(
                self,
                "gate_expired",
                "coordinator",
                "Approval deadline elapsed",
                now,
            );
        }
        Ok(())
    }
    pub fn cancel(&mut self, actor: &str, now: DateTime<Utc>, changes: &mut Changes) -> Result<()> {
        if self.status == JobStatus::Cancelled {
            return Ok(());
        }
        ensure!(!self.status.terminal(), "job already finished");
        changes.effect(self, "stop", Some(self.current().id), None);
        for g in &mut self.gates {
            if g.status == "pending" {
                g.status = "cancelled".into();
            }
        }
        if matches!(self.current().status.as_str(), "queued" | "running") {
            self.current_mut().status = "cancelled".into();
            self.current_mut().finished_at = Some(now);
        }
        self.status = JobStatus::Cancelled;
        self.finished_at = Some(now);
        changes.event(self, "job_cancelled", actor, "Execution cancelled", now);
        Ok(())
    }
    pub fn set_attention_dismissed(
        &mut self,
        dismissed: bool,
        actor: &str,
        now: DateTime<Utc>,
        changes: &mut Changes,
    ) -> Result<()> {
        ensure!(
            matches!(
                self.status,
                JobStatus::Failed
                    | JobStatus::TimedOut
                    | JobStatus::Rejected
                    | JobStatus::Cancelled
            ),
            "only failed, timed out, rejected or cancelled jobs can be dismissed"
        );
        if self.attention_dismissed != dismissed {
            self.attention_dismissed = dismissed;
            changes.event(
                self,
                if dismissed {
                    "attention_dismissed"
                } else {
                    "attention_restored"
                },
                actor,
                if dismissed {
                    "Dismissed from Needs attention"
                } else {
                    "Restored to Needs attention"
                },
                now,
            );
        }
        Ok(())
    }
}
