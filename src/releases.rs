use crate::{
    config::{Platform, Snapshot},
    model::hash,
    store::Store,
    workflow::{identifier, Workflow},
};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// Portable source only: runtime credentials and platform settings are never imported.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub format: u32,
    pub workflow: String,
    pub workflows: BTreeMap<String, String>,
    pub prompts: BTreeMap<String, String>,
    #[serde(default)]
    pub base_generation: i64,
}
impl Bundle {
    pub fn from_snapshot(snapshot: &Snapshot, root: &str) -> Result<Self> {
        let mut pending = vec![root.to_owned()];
        let mut workflows = BTreeMap::new();
        let mut prompts = BTreeMap::new();
        while let Some(id) = pending.pop() {
            if workflows.contains_key(&id) {
                continue;
            }
            let w = snapshot
                .workflows
                .get(&id)
                .context("missing workflow dependency")?;
            workflows.insert(id, serde_yaml::to_string(w)?);
            pending.extend(w.follow_ups.iter().map(|f| f.workflow.clone()));
            for task in w.phases.iter().flat_map(|p| &p.tasks) {
                if let Some(name) = task.with.get("prompt") {
                    prompts.insert(
                        name.clone(),
                        snapshot
                            .prompts
                            .get(name)
                            .context("missing prompt")?
                            .text
                            .clone(),
                    );
                }
            }
        }
        Ok(Self {
            format: 1,
            workflow: root.into(),
            workflows,
            prompts,
            base_generation: 0,
        })
    }
    pub fn resolve(&self, platform: &Platform) -> Result<Snapshot> {
        ensure!(
            self.format == 1 && identifier(&self.workflow),
            "invalid bundle format or workflow"
        );
        ensure!(
            self.workflows.len() <= 100 && self.prompts.len() <= 500,
            "bundle too large"
        );
        let mut definitions = Vec::new();
        for (id, source) in &self.workflows {
            ensure!(source.len() <= 100_000, "workflow too large");
            let w: Workflow = serde_yaml::from_str(source)?;
            ensure!(&w.id == id, "workflow key must match its id");
            definitions.push(w);
        }
        let mut snapshot = platform.resolve(definitions, &self.prompts)?;
        let canonical = Self::from_snapshot(&snapshot, &self.workflow)?;
        ensure!(
            canonical.workflows.len() == self.workflows.len()
                && canonical.prompts.len() == self.prompts.len(),
            "bundle must contain exactly the reachable workflows and prompts"
        );
        let workers: BTreeSet<_> = snapshot
            .workflows
            .values()
            .map(|w| w.defaults.worker_profile.clone())
            .collect();
        let agents: BTreeSet<_> = snapshot
            .workflows
            .values()
            .flat_map(|w| {
                std::iter::once(w.defaults.agent_profile.clone())
                    .chain(w.phases.iter().filter_map(|p| p.agent_profile.clone()))
            })
            .collect();
        let validations: BTreeSet<_> = snapshot
            .workflows
            .values()
            .flat_map(|w| &w.phases)
            .flat_map(|p| &p.tasks)
            .filter(|t| t.uses == "validation.run")
            .filter_map(|t| t.with.get("profile").cloned())
            .collect();
        snapshot.workers.retain(|id, _| workers.contains(id));
        snapshot.agents.retain(|id, _| agents.contains(id));
        snapshot
            .validations
            .retain(|id, _| validations.contains(id));
        snapshot.refresh_hash()?;
        Ok(snapshot)
    }
    pub fn digest(&self) -> Result<String> {
        // YAML formatting does not change release identity; prompt bytes do.
        let workflows: BTreeMap<_, Workflow> = self
            .workflows
            .iter()
            .map(|(id, s)| Ok((id.clone(), serde_yaml::from_str(s)?)))
            .collect::<Result<_>>()?;
        Ok(hash(serde_json::to_vec(
            &json!({"format":self.format,"workflow":self.workflow,"workflows":workflows,"prompts":self.prompts}),
        )?))
    }
}

pub async fn publish(
    store: &Store,
    platform: &Platform,
    bundle: &Bundle,
    actor: &str,
    note: &str,
) -> Result<Uuid> {
    publish_with_options(store, platform, bundle, actor, note, None, false).await
}

pub async fn publish_with_options(
    store: &Store,
    platform: &Platform,
    bundle: &Bundle,
    actor: &str,
    note: &str,
    draft: Option<(Uuid, i64)>,
    activate_now: bool,
) -> Result<Uuid> {
    bundle.resolve(platform)?;
    ensure!(
        !note.trim().is_empty() && note.len() <= 2000,
        "change note required (maximum 2000 characters)"
    );
    let mut tx = store.pool.begin().await?;
    // The lock protects revision-name uniqueness and makes identical imports idempotent.
    sqlx::query("SELECT pg_advisory_xact_lock(7812451)")
        .execute(&mut *tx)
        .await?;
    let current: Option<i64> =
        sqlx::query_scalar("SELECT generation FROM active_releases WHERE workflow=$1")
            .bind(&bundle.workflow)
            .fetch_optional(&mut *tx)
            .await?;
    ensure!(
        current.unwrap_or(0) == bundle.base_generation,
        "conflict: active release changed since export; rebase your draft"
    );
    if let Some((id, revision)) = draft {
        let saved: Option<serde_json::Value> = sqlx::query_scalar("SELECT bundle FROM configuration_drafts WHERE id=$1 AND revision=$2 AND status='editing' FOR UPDATE")
            .bind(id).bind(revision).fetch_optional(&mut *tx).await?;
        let saved: Bundle = serde_json::from_value(
            saved.context("conflict: draft changed or is already published")?,
        )?;
        ensure!(
            saved.digest()? == bundle.digest()?,
            "conflict: publish the exact saved candidate"
        );
    }
    let mut revisions = Vec::new();
    for (name, content) in &bundle.prompts {
        revisions.push(("prompt", name.clone(), content.clone()));
    }
    for source in bundle.workflows.values() {
        let w: Workflow = serde_yaml::from_str(source)?;
        revisions.push((
            "workflow",
            format!("{}@{}", w.id, w.version),
            serde_json::to_string(&w)?,
        ));
    }
    for (kind, name, content) in revisions {
        let digest = hash(&content);
        let old: Option<String> = sqlx::query_scalar(
            "SELECT digest FROM configuration_revisions WHERE kind=$1 AND name=$2",
        )
        .bind(kind)
        .bind(&name)
        .fetch_optional(&mut *tx)
        .await?;
        ensure!(
            old.as_ref().is_none_or(|v| v == &digest),
            "revision {name} already exists with different content; increment its version"
        );
        sqlx::query("INSERT INTO configuration_revisions(kind,name,digest,content,actor,note) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING").bind(kind).bind(name).bind(digest).bind(content).bind(actor).bind(note).execute(&mut *tx).await?;
    }
    let id: Uuid = sqlx::query_scalar("INSERT INTO workflow_releases(id,workflow,digest,bundle,actor,note,release_number) VALUES($1,$2,$3,$4,$5,$6,(SELECT COALESCE(MAX(release_number),0)+1 FROM workflow_releases WHERE workflow=$2)) ON CONFLICT(workflow,digest) DO UPDATE SET digest=EXCLUDED.digest RETURNING id")
        .bind(Uuid::new_v4()).bind(&bundle.workflow).bind(bundle.digest()?).bind(serde_json::to_value(bundle)?).bind(actor).bind(note).fetch_one(&mut *tx).await?;
    if activate_now {
        let generation = bundle.base_generation + 1;
        sqlx::query("INSERT INTO active_releases(workflow,release_id,generation) VALUES($1,$2,$3) ON CONFLICT(workflow) DO UPDATE SET release_id=$2,generation=$3").bind(&bundle.workflow).bind(id).bind(generation).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO release_audit(workflow,release_id,generation,actor,note) VALUES($1,$2,$3,$4,$5)").bind(&bundle.workflow).bind(id).bind(generation).bind(actor).bind(note).execute(&mut *tx).await?;
    }
    if let Some((draft_id, _)) = draft {
        sqlx::query("UPDATE configuration_drafts SET status='published',published_release=$2,updated_at=now() WHERE id=$1").bind(draft_id).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(id)
}
pub async fn get(store: &Store, id: Uuid) -> Result<Bundle> {
    let value: Value = sqlx::query_scalar("SELECT bundle FROM workflow_releases WHERE id=$1")
        .bind(id)
        .fetch_one(&store.pool)
        .await?;
    Ok(serde_json::from_value(value)?)
}
pub async fn activate(
    store: &Store,
    platform: &Platform,
    id: Uuid,
    expected: i64,
    actor: &str,
    note: &str,
) -> Result<i64> {
    let bundle = get(store, id).await?;
    bundle.resolve(platform)?;
    ensure!(
        !note.trim().is_empty() && note.len() <= 2000,
        "activation or restore note required"
    );
    let mut tx = store.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7812451)")
        .execute(&mut *tx)
        .await?;
    let current: Option<i64> =
        sqlx::query_scalar("SELECT generation FROM active_releases WHERE workflow=$1 FOR UPDATE")
            .bind(&bundle.workflow)
            .fetch_optional(&mut *tx)
            .await?;
    ensure!(
        current.unwrap_or(0) == expected,
        "conflict: active release changed; refresh before activating"
    );
    let generation = expected + 1;
    sqlx::query("INSERT INTO active_releases(workflow,release_id,generation) VALUES($1,$2,$3) ON CONFLICT(workflow) DO UPDATE SET release_id=$2,generation=$3").bind(&bundle.workflow).bind(id).bind(generation).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO release_audit(workflow,release_id,generation,actor,note) VALUES($1,$2,$3,$4,$5)").bind(&bundle.workflow).bind(id).bind(generation).bind(actor).bind(note).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(generation)
}
pub async fn active(store: &Store, platform: &Platform, workflow: &str) -> Result<Snapshot> {
    let (id, value): (Uuid, Value) = sqlx::query_as("SELECT r.id,r.bundle FROM active_releases a JOIN workflow_releases r ON r.id=a.release_id WHERE a.workflow=$1").bind(workflow).fetch_optional(&store.pool).await?.context("workflow has no active release")?;
    let bundle: Bundle = serde_json::from_value(value)?;
    let mut snapshot = bundle.resolve(platform)?;
    snapshot.release_id = Some(id);
    snapshot.release_digest = Some(bundle.digest()?);
    snapshot.refresh_hash()?;
    Ok(snapshot)
}
pub async fn catalog(store: &Store) -> Result<BTreeMap<String, Workflow>> {
    let rows: Vec<(String, Value)> = sqlx::query_as("SELECT a.workflow,r.bundle FROM active_releases a JOIN workflow_releases r ON r.id=a.release_id").fetch_all(&store.pool).await?;
    rows.into_iter()
        .map(|(id, v)| {
            let b: Bundle = serde_json::from_value(v)?;
            Ok((
                id.clone(),
                serde_yaml::from_str(b.workflows.get(&id).context("missing root")?)?,
            ))
        })
        .collect()
}
pub async fn seed(store: &Store, platform: &Platform, snapshot: &Snapshot) -> Result<()> {
    let mut tx = store.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7812451)")
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_releases")
        .fetch_one(&mut *tx)
        .await?;
    if count > 0 {
        return Ok(());
    }
    for root in snapshot.workflows.keys() {
        let bundle = Bundle::from_snapshot(snapshot, root)?;
        bundle.resolve(platform)?;
        let id = Uuid::new_v4();
        for (name, content) in &bundle.prompts {
            sqlx::query("INSERT INTO configuration_revisions(kind,name,digest,content,actor,note) VALUES('prompt',$1,$2,$3,'bootstrap','Imported bundled definitions') ON CONFLICT DO NOTHING").bind(name).bind(hash(content)).bind(content).execute(&mut *tx).await?;
        }
        for source in bundle.workflows.values() {
            let w: Workflow = serde_yaml::from_str(source)?;
            let content = serde_json::to_string(&w)?;
            sqlx::query("INSERT INTO configuration_revisions(kind,name,digest,content,actor,note) VALUES('workflow',$1,$2,$3,'bootstrap','Imported bundled definitions') ON CONFLICT DO NOTHING").bind(format!("{}@{}",w.id,w.version)).bind(hash(&content)).bind(content).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO workflow_releases(id,workflow,digest,bundle,actor,note,release_number) VALUES($1,$2,$3,$4,'bootstrap','Imported bundled definitions',1)").bind(id).bind(root).bind(bundle.digest()?).bind(serde_json::to_value(&bundle)?).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO active_releases(workflow,release_id,generation) VALUES($1,$2,1)")
            .bind(root)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO release_audit(workflow,release_id,generation,actor,note) VALUES($1,$2,1,'bootstrap','Initial activation')").bind(root).bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Assign new immutable names only where content differs from a published revision.
/// Publication rechecks uniqueness; a concurrent publisher requires a fresh review.
pub async fn prepare(store: &Store, platform: &Platform, mut bundle: Bundle) -> Result<Bundle> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT kind,name,content FROM configuration_revisions")
            .fetch_all(&store.pool)
            .await?;
    let mut definitions: BTreeMap<String, Workflow> = bundle
        .workflows
        .iter()
        .map(|(id, s)| Ok((id.clone(), serde_yaml::from_str(s)?)))
        .collect::<Result<_>>()?;
    let original_prompts = bundle.prompts.clone();
    for (name, content) in original_prompts {
        let Some((stem, _)) = name.split_once('@') else {
            anyhow::bail!("versioned prompt name required");
        };
        ensure!(identifier(stem), "invalid prompt name");
        if rows
            .iter()
            .any(|(k, n, c)| k == "prompt" && n == &name && c != &content)
        {
            let identical = rows.iter().find(|(k, n, c)| {
                k == "prompt" && n.split_once('@').is_some_and(|(s, _)| s == stem) && c == &content
            });
            let next = if let Some((_, n, _)) = identical {
                n.clone()
            } else {
                let version = rows
                    .iter()
                    .filter(|(k, _, _)| k == "prompt")
                    .map(|(_, n, _)| n)
                    .chain(bundle.prompts.keys())
                    .filter_map(|n| n.split_once('@'))
                    .filter(|(s, _)| *s == stem)
                    .filter_map(|(_, v)| v.parse::<u32>().ok())
                    .max()
                    .unwrap_or(0)
                    .checked_add(1)
                    .context("prompt version exhausted")?;
                format!("{stem}@{version}")
            };
            ensure!(
                bundle.prompts.get(&next).is_none_or(|c| c == &content),
                "conflicting prompt candidates"
            );
            bundle.prompts.remove(&name);
            bundle.prompts.insert(next.clone(), content);
            for w in definitions.values_mut() {
                for task in w.phases.iter_mut().flat_map(|p| &mut p.tasks) {
                    if task.with.get("prompt") == Some(&name) {
                        task.with.insert("prompt".into(), next.clone());
                    }
                }
            }
        }
    }
    for (id, w) in &mut definitions {
        let content = serde_json::to_string(w)?;
        let name = format!("{id}@{}", w.version);
        if rows
            .iter()
            .any(|(k, n, c)| k == "workflow" && n == &name && c != &content)
        {
            let latest = rows
                .iter()
                .filter(|(k, _, _)| k == "workflow")
                .filter_map(|(_, n, _)| n.split_once('@'))
                .filter(|(s, _)| s == id)
                .filter_map(|(_, v)| v.parse::<u32>().ok())
                .max()
                .unwrap_or(0);
            w.version = latest
                .max(w.version)
                .checked_add(1)
                .context("workflow version exhausted")?;
        }
    }
    bundle.workflows = definitions
        .iter()
        .map(|(id, w)| Ok((id.clone(), serde_yaml::to_string(w)?)))
        .collect::<Result<_>>()?;
    bundle.resolve(platform)?;
    Ok(bundle)
}
