use chrono::{Duration, Utc};
use factories::{config::Platform, model::*, workflow::seconds};
use std::path::Path;
use uuid::Uuid;

#[test]
fn dismissing_attention_is_durable_idempotent_and_preserves_outcome() {
    let mut j = job();
    let mut changes = Changes::default();
    assert!(j
        .set_attention_dismissed(true, "operator", Utc::now(), &mut changes)
        .is_err());
    for status in [
        JobStatus::Failed,
        JobStatus::TimedOut,
        JobStatus::Rejected,
        JobStatus::Cancelled,
    ] {
        j.status = status.clone();
        j.attention_dismissed = false;
        changes.events.clear();
        j.set_attention_dismissed(true, "operator", Utc::now(), &mut changes)
            .unwrap();
        j.set_attention_dismissed(true, "operator", Utc::now(), &mut changes)
            .unwrap();
        assert_eq!(changes.events.len(), 1);
        assert_eq!(j.status, status);
        let persisted: Job = serde_json::from_value(serde_json::to_value(&j).unwrap()).unwrap();
        assert!(persisted.attention_dismissed);
        j.set_attention_dismissed(false, "operator", Utc::now(), &mut changes)
            .unwrap();
        assert!(!j.attention_dismissed);
        assert_eq!(changes.events.len(), 2);
        assert!(changes.effects.is_empty());
    }
    let mut legacy = serde_json::to_value(&j).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("attention_dismissed");
    assert!(
        !serde_json::from_value::<Job>(legacy)
            .unwrap()
            .attention_dismissed
    );
}

fn job() -> Job {
    let config = Platform::load(Path::new("config/platform.yaml")).unwrap();
    let snapshot = config
        .snapshot(Path::new("workflows"), Path::new("prompts"))
        .unwrap();
    let id = Uuid::new_v4();
    let now = Utc::now();
    let mut job = Job {
        id,
        case_id: Uuid::new_v4(),
        parent_id: None,
        root_id: id,
        depth: 0,
        workflow: "demo".into(),
        snapshot,
        repository: Repository {
            id: "local-demo".into(),
            url: "fixture://local-demo".into(),
            revision: "0".repeat(40),
            base_branch: "main".into(),
            work_branch: format!("factory/{id}"),
            provider: "fixture".into(),
            api_url: "fixture://local-demo".into(),
        },
        issue: Issue {
            ticket: None,
            provider: "fixture".into(),
            key: "TEST-1".into(),
            title: "test".into(),
            body: "test".into(),
            url: None,
        },
        status: JobStatus::Queued,
        phase_index: 0,
        attempts: vec![],
        gates: vec![],
        upstream_artifacts: Default::default(),
        created_at: now,
        finished_at: None,
        requested_by: "test".into(),
        attention_dismissed: false,
    };
    job.schedule(now, &mut Changes::default());
    job
}
fn finish(job: &mut Job) -> (Uuid, Uuid, Completion) {
    let now = Utc::now();
    let attempt = job.current().id;
    let instance = Uuid::new_v4();
    job.claim(attempt, instance, now, &mut Changes::default())
        .unwrap();
    let published: Vec<_> = job
        .phase()
        .tasks
        .iter()
        .filter(|t| t.uses == "artifact.publish")
        .map(|t| t.with["name"].clone())
        .collect();
    for name in published {
        job.current_mut().artifacts.insert(
            name.clone(),
            Artifact {
                id: Uuid::new_v4(),
                attempt_id: attempt,
                name,
                sha256: hash("artifact version 1"),
                size: 18,
                created_at: now,
            },
        );
    }
    let result = Completion {
        agent_runs: vec![],
        succeeded: true,
        tasks: job
            .phase()
            .tasks
            .iter()
            .map(|t| TaskResult {
                task: t.uses.clone(),
                status: "succeeded".into(),
                duration_ms: 100,
                summary: "done".into(),
            })
            .collect(),
        findings: 0,
        pull_request: None,
        revision: None,
        error: None,
    };
    job.complete(
        attempt,
        instance,
        result.clone(),
        now,
        &mut Changes::default(),
    )
    .unwrap();
    (attempt, instance, result)
}
#[test]
fn timeout_reports_whether_the_worker_started() {
    for started in [false, true] {
        let mut j = job();
        if started {
            j.claim(
                j.current().id,
                Uuid::new_v4(),
                Utc::now(),
                &mut Changes::default(),
            )
            .unwrap();
        }
        let mut changes = Changes::default();
        j.reconcile(j.current().deadline, &mut changes).unwrap();
        let failed = &j.attempts[0];
        assert_eq!(failed.status, "timed_out");
        let prefix = if started {
            "Phase execution timed out:"
        } else {
            "Worker launch timed out:"
        };
        assert!(failed.error.as_ref().unwrap().starts_with(prefix));
        assert!(changes
            .events
            .iter()
            .any(|e| e.kind == "attempt_failed" && e.message.starts_with(prefix)));
        assert_eq!(j.current().number, 2);
    }
}
#[test]
fn gate_releases_worker_and_approval_schedules_a_fresh_attempt() {
    let mut j = job();
    let (first, _, _) = finish(&mut j);
    assert_eq!(j.status, JobStatus::AwaitingApproval);
    assert_eq!(j.attempts.len(), 1);
    assert_eq!(j.current().status, "succeeded");
    let gate = j.gates[0].clone();
    let decision = Decision {
        event_id: "approval-1".into(),
        gate_id: gate.id,
        artifact_digest: gate.artifact_digest,
        approve: true,
    };
    let mut changes = Changes::default();
    j.decide(&decision, "maintainer", "api", Utc::now(), &mut changes)
        .unwrap();
    assert_eq!(j.status, JobStatus::Queued);
    assert_eq!(j.phase().id, "plan");
    assert_ne!(j.current().id, first);
    assert_eq!(
        changes.effects.iter().filter(|e| e.kind == "start").count(),
        1
    );
}
#[test]
fn wrong_artifact_and_wrong_channel_cannot_approve() {
    let mut j = job();
    finish(&mut j);
    let g = j.gates[0].clone();
    let mut d = Decision {
        event_id: "wrong".into(),
        gate_id: g.id,
        artifact_digest: hash("different"),
        approve: true,
    };
    assert!(j
        .decide(&d, "maintainer", "api", Utc::now(), &mut Changes::default())
        .is_err());
    d.artifact_digest = g.artifact_digest;
    assert!(j
        .decide(
            &d,
            "maintainer",
            "slack",
            Utc::now(),
            &mut Changes::default()
        )
        .is_err());
    assert_eq!(j.status, JobStatus::AwaitingApproval);
}
#[test]
fn duplicate_decision_is_idempotent_but_conflicting_replay_is_rejected() {
    let mut j = job();
    finish(&mut j);
    let gate = j.gates[0].clone();
    let mut d = Decision {
        event_id: "event-1".into(),
        gate_id: gate.id,
        artifact_digest: gate.artifact_digest,
        approve: true,
    };
    j.decide(&d, "maintainer", "api", Utc::now(), &mut Changes::default())
        .unwrap();
    let attempts = j.attempts.len();
    let mut changes = Changes::default();
    j.decide(&d, "maintainer", "api", Utc::now(), &mut changes)
        .unwrap();
    assert_eq!(attempts, j.attempts.len());
    assert!(changes.effects.is_empty());
    d.approve = false;
    assert!(j
        .decide(&d, "maintainer", "api", Utc::now(), &mut changes)
        .is_err());
}
#[test]
fn stale_worker_cannot_complete_retried_attempt() {
    let mut j = job();
    let a = j.current().id;
    let instance = Uuid::new_v4();
    j.claim(a, instance, Utc::now(), &mut Changes::default())
        .unwrap();
    let late = Completion {
        agent_runs: vec![],
        succeeded: false,
        tasks: vec![],
        findings: 0,
        pull_request: None,
        revision: None,
        error: Some("old failure".into()),
    };
    j.fail(
        "worker disappeared",
        false,
        Utc::now(),
        &mut Changes::default(),
    )
    .unwrap();
    assert_ne!(a, j.current().id);
    assert!(j
        .complete(a, instance, late, Utc::now(), &mut Changes::default())
        .is_err());
    assert_eq!(j.attempts.len(), 2);
}
#[test]
fn duplicate_completion_does_not_create_another_gate() {
    let mut j = job();
    let (a, instance, result) = finish(&mut j);
    let mut changes = Changes::default();
    j.complete(a, instance, result, Utc::now(), &mut changes)
        .unwrap();
    assert_eq!(j.gates.len(), 1);
    assert!(changes.effects.is_empty());
}
#[test]
fn second_worker_instance_cannot_claim_same_attempt() {
    let mut j = job();
    let a = j.current().id;
    let first = Uuid::new_v4();
    j.claim(a, first, Utc::now(), &mut Changes::default())
        .unwrap();
    assert!(j
        .claim(a, Uuid::new_v4(), Utc::now(), &mut Changes::default())
        .is_err());
    assert_eq!(j.current().instance, Some(first));
}
#[test]
fn deadline_is_enforced_even_before_reconciliation() {
    let mut j = job();
    let a = j.current().id;
    assert!(j
        .claim(
            a,
            Uuid::new_v4(),
            j.current().deadline,
            &mut Changes::default()
        )
        .is_err());
    finish(&mut j);
    let g = j.gates[0].clone();
    assert!(j
        .decide(
            &Decision {
                event_id: "late".into(),
                gate_id: g.id,
                artifact_digest: g.artifact_digest,
                approve: true
            },
            "maintainer",
            "api",
            g.deadline,
            &mut Changes::default()
        )
        .is_err());
}
#[test]
fn retry_budget_is_finite() {
    let mut j = job();
    j.fail("failure", false, Utc::now(), &mut Changes::default())
        .unwrap();
    j.fail("failure", false, Utc::now(), &mut Changes::default())
        .unwrap();
    assert_eq!(j.status, JobStatus::Failed);
    assert_eq!(j.attempts.len(), 2);
}
#[test]
fn pending_gate_survives_serialization_and_expires_without_worker() {
    let mut j = job();
    finish(&mut j);
    let bytes = serde_json::to_vec(&j).unwrap();
    let mut restored: Job = serde_json::from_slice(&bytes).unwrap();
    let mut changes = Changes::default();
    restored
        .reconcile(j.gates[0].deadline + Duration::seconds(1), &mut changes)
        .unwrap();
    assert_eq!(restored.status, JobStatus::TimedOut);
    assert_eq!(restored.gates[0].status, "expired");
    assert!(changes.effects.is_empty());
}
#[test]
fn cancellation_fences_worker_and_pending_gate() {
    let mut j = job();
    let a = j.current().id;
    let instance = Uuid::new_v4();
    j.claim(a, instance, Utc::now(), &mut Changes::default())
        .unwrap();
    j.cancel("operator", Utc::now(), &mut Changes::default())
        .unwrap();
    assert!(j.worker(a, instance, Utc::now()).is_err());
    let mut j = job();
    finish(&mut j);
    j.cancel("operator", Utc::now(), &mut Changes::default())
        .unwrap();
    assert_eq!(j.gates[0].status, "cancelled");
}
#[test]
fn missing_artifact_cannot_finish_successfully() {
    let mut j = job();
    let a = j.current().id;
    let instance = Uuid::new_v4();
    j.claim(a, instance, Utc::now(), &mut Changes::default())
        .unwrap();
    let result = Completion {
        agent_runs: vec![],
        succeeded: true,
        tasks: j
            .phase()
            .tasks
            .iter()
            .map(|t| TaskResult {
                task: t.uses.clone(),
                status: "succeeded".into(),
                duration_ms: 1,
                summary: "done".into(),
            })
            .collect(),
        findings: 0,
        pull_request: None,
        revision: None,
        error: None,
    };
    assert!(j
        .complete(a, instance, result, Utc::now(), &mut Changes::default())
        .is_err());
}
#[test]
fn invalid_definitions_are_rejected() {
    let base = job().snapshot.workflows["demo"].clone();
    let mut w = base.clone();
    w.phases[0].tasks[0].uses = "shell.arbitrary".into();
    assert!(w.validate().is_err());
    let mut w = base.clone();
    w.phases[1].inputs.insert(
        "research".into(),
        "phases.implement.artifacts.future".into(),
    );
    assert!(w.validate().is_err());
    let mut w = base.clone();
    w.phases[0].max_attempts = 100;
    assert!(w.validate().is_err());
    let mut w = base;
    w.phases[0].tasks[0]
        .with
        .insert("output".into(), "../../secret".into());
    assert!(w.validate().is_err());
}

#[test]
fn openhands_success_requires_matching_execution_provenance() {
    let mut j = job();
    // A fully formed completion and artifacts, created by the fixture helper.
    let (_, _, mut result) = finish(&mut j);
    let mut j = job();
    j.snapshot.workflows.get_mut("demo").unwrap().phases[0].agent_profile =
        Some("coding-default".into());
    let attempt = j.current().id;
    let instance = Uuid::new_v4();
    j.claim(attempt, instance, Utc::now(), &mut Changes::default())
        .unwrap();
    let error = j
        .complete(
            attempt,
            instance,
            result.clone(),
            Utc::now(),
            &mut Changes::default(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("provenance"));
    let config = j.snapshot.agents["coding-default"]
        .openhands
        .as_ref()
        .unwrap();
    result.agent_runs.push(AgentRun {
        harness: "openhands".into(),
        harness_version: config.sdk_version.clone(),
        model: "wrong/model".into(),
        status: "finished".into(),
        prompt_tokens: 1,
        completion_tokens: 1,
    });
    let error = j
        .complete(
            attempt,
            instance,
            result.clone(),
            Utc::now(),
            &mut Changes::default(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("provenance"));
    result.agent_runs[0].model = j.snapshot.agents["coding-default"]
        .openhands
        .as_ref()
        .unwrap()
        .model
        .clone();
    // Provenance now passes, but the independent artifact requirement still applies.
    let error = j
        .complete(
            attempt,
            instance,
            result,
            Utc::now(),
            &mut Changes::default(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("missing published artifact"));
}
#[test]
fn duration_parsing_rejects_zero_overflow_and_unbounded_values() {
    assert_eq!(seconds("250m").unwrap(), 15000);
    for bad in [
        "",
        "0s",
        "-1m",
        "90x",
        "1秒",
        "99999999999999999999999999h",
        "31d",
    ] {
        assert!(seconds(bad).is_err(), "{bad}");
    }
}
#[test]
fn snapshot_digest_survives_roundtrip_and_detects_prompt_changes() {
    let pinned = job().snapshot;
    let mut restored: factories::config::Snapshot =
        serde_json::from_slice(&serde_json::to_vec(&pinned).unwrap()).unwrap();
    restored.refresh_hash().unwrap();
    assert_eq!(pinned.definition_hash, restored.definition_hash);
    restored.prompts.get_mut("research@3").unwrap().text = "changed".into();
    restored.refresh_hash().unwrap();
    assert_ne!(pinned.definition_hash, restored.definition_hash);
}
#[test]
fn followups_preserve_case_and_pass_parent_artifacts_with_bounded_depth() {
    let mut j = job();
    j.workflow = "pr-review".into();
    j.snapshot
        .workflows
        .get_mut("pr-review")
        .unwrap()
        .defaults
        .agent_profile = "fixture".into();
    j.attempts.clear();
    j.schedule(Utc::now(), &mut Changes::default());
    let a = j.current().id;
    let instance = Uuid::new_v4();
    j.claim(a, instance, Utc::now(), &mut Changes::default())
        .unwrap();
    for name in ["review", "findings", "pr-context", "pr-diff"] {
        j.current_mut().artifacts.insert(
            name.into(),
            Artifact {
                id: Uuid::new_v4(),
                attempt_id: a,
                name: name.into(),
                sha256: hash("findings"),
                size: 8,
                created_at: Utc::now(),
            },
        );
    }
    let result = Completion {
        agent_runs: vec![],
        succeeded: true,
        tasks: j
            .phase()
            .tasks
            .iter()
            .map(|t| TaskResult {
                task: t.uses.clone(),
                status: "succeeded".into(),
                duration_ms: 1,
                summary: "done".into(),
            })
            .collect(),
        findings: 2,
        pull_request: None,
        revision: None,
        error: None,
    };
    let mut changes = Changes::default();
    j.complete(a, instance, result, Utc::now(), &mut changes)
        .unwrap();
    let child = changes.follow_up.unwrap();
    assert_eq!(child.case_id, j.case_id);
    assert_eq!(child.parent_id, Some(j.id));
    assert_eq!(child.workflow, "pr-review-fix");
    assert!(child.upstream_artifacts.contains_key("review"));
    let mut bounded = j.clone();
    bounded.depth = 6;
    let mut changes = Changes::default();
    bounded.advance(Utc::now(), &mut changes).unwrap();
    assert!(changes.follow_up.is_none());
}
