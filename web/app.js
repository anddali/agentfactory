"use strict";
const $ = (id) => document.getElementById(id);
const escapeHTML = (value) =>
  String(value ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
const names = {
  connectors: ["Connectors", "Manage service connections and credentials for your repositories."],
  overview: [
    "Operations overview",
    "A clear view of the work, the workers, and what needs you.",
  ],
  floor: [
    "The Factory Floor",
    "Cases move forward. People make the decisions.",
  ],
  lines: [
    "Production lines",
    "Versioned workflows, explicit permissions, and durable human gates.",
  ],
  jobs: [
    "Jobs & receipts",
    "Every execution, attempt, and decision has a record.",
  ],
  gates: [
    "Human gates",
    "Review the exact work awaiting a decision in its configured channel.",
  ],
  activity: [
    "Activity",
    "The durable event history of your engineering workforce.",
  ],
  platform: [
    "Platform",
    "The control plane remembers. Workers execute and exit.",
  ],
};
let state = { jobs: [], events: [], catalog: null, loaded: false },
  selectedJob = null,
  detailTab = "timeline",
  detailData = null,
  busy = false;
let accessVersion = 0,
  refreshQueued = false;
let access = { subject: null, approvable_repositories: [] },
  pendingDecision = null,
  decisionBusy = false;
const decisionIds = new Map();
function setHTML(element, html) {
  if (element._renderedHTML === html) return;
  const active = element.contains(document.activeElement)
    ? document.activeElement
    : null;
  const key =
    active && ["job", "artifact", "receipt"].find((k) => active.dataset[k]);
  const value = key && active.dataset[key];
  element.innerHTML = html;
  element._renderedHTML = html;
  if (key)
    element
      .querySelector(`[data-${key}="${CSS.escape(value)}"]`)
      ?.focus({ preventScroll: true });
}
function resetAccess() {
  accessVersion++;
  clearJiraSearch();
  if ($("launch").open) $("launch").close();
  pendingLaunch = null;
  $("launch-button").disabled = true;
  connectorCatalog = [];
  if ($("connector-editor").open) $("connector-editor").close();
  access = { subject: null, approvable_repositories: [] };
  if ($("decision").open) $("decision").close();
  $("access-mode").textContent = "◉ READ-ONLY";
  state = { jobs: [], events: [], catalog: null, loaded: false };
  if ($("detail").open) $("detail").close();
  setHTML($("content"), empty("Connecting with the new access settings"));
  refresh();
}
const short = (id) => String(id).slice(0, 8),
  label = (status) => String(status).replaceAll("_", " "),
  date = (value) =>
    value
      ? new Date(value).toLocaleString([], {
          month: "short",
          day: "numeric",
          hour: "2-digit",
          minute: "2-digit",
        })
      : "—";
const age = (value) => {
  const seconds = Math.max(
    0,
    Math.floor((Date.now() - new Date(value)) / 1000),
  );
  return seconds < 60
    ? `${seconds}s`
    : seconds < 3600
      ? `${Math.floor(seconds / 60)}m`
      : `${Math.floor(seconds / 3600)}h`;
};
const badge = (status) =>
  `<span class="badge ${escapeHTML(status)}">${escapeHTML(label(status))}</span>`;
const empty = (title, body = "") =>
  `<div class="empty-state"><b>${escapeHTML(title)}</b>${escapeHTML(body)}</div>`;
const jobButton = (job, text) =>
  `<button data-job="${escapeHTML(job.id)}">${escapeHTML(text || job.issue.key)}</button>`;
function page() {
  const p = location.hash.slice(1);
  return Object.hasOwn(names, p) ? p : "overview";
}
function reportLink(jobId, artifactId) {
  return `#report/${encodeURIComponent(jobId)}/${encodeURIComponent(artifactId)}`;
}
async function openReport(jobId, artifactId) {
  try {
    const data = await api(`/api/jobs/${encodeURIComponent(jobId)}`);
    const artifact = data.job.attempts.flatMap(a => Object.values(a.artifacts)).find(a => a.id === artifactId);
    if (!artifact) throw new Error("Report not found");
    const token = sessionStorage.getItem("factory-observer-token");
    const response = await fetch(`/api/artifacts/${encodeURIComponent(artifactId)}`, {
      headers: token ? { Authorization: `Bearer ${token}` } : {},
    });
    if (!response.ok) throw new Error(`Report unavailable (${response.status})`);
    $("report-title").textContent = artifact.name;
    $("report-body").innerHTML = renderMarkdown(await response.text());
    if (!$("report").open) $("report").showModal();
  } catch (error) { toast(error.message); }
}
function openReportHash() {
  const match = /^#report\/([0-9a-f-]{36})\/([0-9a-f-]{36})$/i.exec(location.hash);
  if (match) openReport(match[1], match[2]);
}
function renderMarkdown(source) {
  const inline = value => escapeHTML(value).replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g,
    (_, label, url) => `<a href="${url.replace(/&amp;/g, '&amp;')}" target="_blank" rel="noopener noreferrer">${label}</a>`)
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/\*([^*]+)\*/g, '<em>$1</em>');
  const lines = String(source).replace(/\r\n?/g, '\n').split('\n');
  let html = '', inCode = false, list = '', table = false;
  const closeList = () => { if (list) { html += `</${list}>`; list = ''; } };
  const closeTable = () => { if (table) { html += '</tbody></table>'; table = false; } };
  const cells = line => line.trim().replace(/^\||\|$/g, '').split('|').map(x => x.trim());
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (/^\s*```/.test(line)) { closeList(); closeTable(); html += inCode ? '</code></pre>' : '<pre><code>'; inCode = !inCode; continue; }
    if (inCode) { html += `${escapeHTML(line)}\n`; continue; }
    if (!line.trim()) { closeList(); closeTable(); continue; }
    if (!table && line.includes('|') && /^\s*\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(lines[i + 1] || '')) {
      closeList(); table = true;
      html += `<table><thead><tr>${cells(line).map(x => `<th>${inline(x)}</th>`).join('')}</tr></thead><tbody>`;
      i++; continue;
    }
    if (table && line.includes('|')) { html += `<tr>${cells(line).map(x => `<td>${inline(x)}</td>`).join('')}</tr>`; continue; }
    closeTable();
    const heading = /^(#{1,6})\s+(.+)$/.exec(line);
    if (heading) { closeList(); const level = heading[1].length; html += `<h${level}>${inline(heading[2])}</h${level}>`; continue; }
    const item = /^\s*([-*]|\d+\.)\s+(.+)$/.exec(line);
    if (item) { const kind = item[1].endsWith('.') ? 'ol' : 'ul'; if (list !== kind) { closeList(); html += `<${kind}>`; list = kind; } html += `<li>${inline(item[2])}</li>`; continue; }
    closeList();
    if (/^\s*---+\s*$/.test(line)) html += '<hr>';
    else if (/^>\s?/.test(line)) html += `<blockquote>${inline(line.replace(/^>\s?/, ''))}</blockquote>`;
    else html += `<p>${inline(line)}</p>`;
  }
  closeList();
  closeTable();
  if (inCode) html += '</code></pre>';
  return html;
}
function filteredJobs() {
  const q = $("search").value.trim().toLowerCase();
  return state.jobs.filter(
    (j) =>
      (!$("repository").value || j.repository === $("repository").value) &&
      (!$("workflow").value || j.workflow === $("workflow").value) &&
      (!q ||
        [j.issue.key, j.issue.title, j.repository, j.workflow, j.id]
          .join(" ")
          .toLowerCase()
          .includes(q)),
  );
}
function cases(jobs) {
  const map = new Map();
  for (const j of jobs) {
    if (!map.has(j.case_id)) map.set(j.case_id, j);
  }
  return [...map.values()];
}
function counts(jobs) {
  const latest = cases(jobs);
  return {
    working: latest.filter((j) => ["queued", "running"].includes(j.status))
      .length,
    human: latest.filter((j) => j.status === "awaiting_approval").length,
    delivered: latest.filter((j) => j.status === "succeeded").length,
    minutes: jobs.reduce((n, j) => n + j.agent_ms, 0) / 60000,
  };
}
async function api(path) {
  const token = sessionStorage.getItem("factory-observer-token");
  const response = await fetch(path, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
    cache: "no-store",
  });
  if (!response.ok) {
    let message = `Control plane returned ${response.status}`;
    try {
      message = (await response.json()).error || message;
    } catch {}
    throw new Error(message);
  }
  return response.json();
}
function route() {
  const current = page();
  $("page-title").textContent = names[current][0];
  $("breadcrumb").textContent =
    current === "floor" ? "Factory floor" : names[current][0];
  $("page-description").textContent = names[current][1];
  $("page-eyebrow").textContent =
    current === "platform"
      ? "DURABLE CONTROL PLANE"
      : "YOUR ENGINEERING WORKFORCE";
  document.title = `${names[current][0]} · Factories`;
  document
    .querySelectorAll("[data-nav]")
    .forEach((a) =>
      a.setAttribute(
        "aria-current",
        a.dataset.nav === current ? "page" : "false",
      ),
    );
  render();
}
function stats(jobs) {
  const c = counts(jobs);
  return `<div class="stats"><div class="stat"><div class="stat-label">In flight</div><div class="stat-value">${c.working}</div><small>Active cases across production lines</small></div><div class="stat"><div class="stat-label">Awaiting human decision</div><div class="stat-value amber">${c.human}</div><small>Durable gates · no worker waiting</small></div><div class="stat"><div class="stat-label">Delivered</div><div class="stat-value green">${c.delivered}</div><small>Completed cases in this view</small></div><div class="stat"><div class="stat-label">Recorded agent minutes</div><div class="stat-value">${c.minutes.toFixed(1)}</div><small>Reported attempts, including failures</small></div></div>`;
}
function milestones(events, limit = 8) {
  return (
    events
      .slice(0, limit)
      .map(
        (e) =>
          `<div class="activity-item ${e.kind.includes("approval") ? "gate" : e.kind.includes("failed") ? "bad" : e.kind.includes("succeeded") ? "good" : ""}"><span class="dot"></span><div><p>${escapeHTML(label(e.kind))} · ${escapeHTML(e.message)}</p><time>${date(e.at)} · ${escapeHTML(e.actor)} · <button data-job="${e.job_id}">${short(e.job_id)}</button></time></div></div>`,
      )
      .join("") ||
    '<div class="panel-body subtle">No recorded activity yet.</div>'
  );
}
function overview(jobs, events) {
  const c = counts(jobs),
    latest = cases(jobs),
    queued = latest.filter((j) => j.status === "queued").length;
  return `${stats(jobs)}<div class="overview-grid"><div class="panel"><div class="panel-head"><h2>Case flow</h2><a href="#floor">Open factory floor ↗</a></div><div class="hero-flow">${[
    ["Intake", queued, ""],
    ["Working", c.working - queued, ""],
    ["Human decision", c.human, "wait"],
    ["Delivered", c.delivered, "done"],
  ]
    .map(
      ([name, n, cls], i) =>
        `${i ? '<span class="flow-arrow">→</span>' : ""}<div class="flow-node ${cls}"><span>${name}</span><strong>${n}</strong><div class="bar"></div></div>`,
    )
    .join(
      "",
    )}</div><div class="panel-note">Each case keeps its identity across retries and follow-up jobs. Approvals refer to a specific artifact version.</div></div><div class="panel"><div class="panel-head"><h2>Latest activity</h2><a href="#activity">View all →</a></div>${milestones(events, 3)}</div></div><div class="section-heading"><h2>Production lines</h2><a href="#lines">Explore workflows →</a></div><div class="line-grid">${Object.values(
    state.catalog.workflows,
  )
    .filter((w) => !$("workflow").value || w.id === $("workflow").value)
    .map((w) => lineCard(w, jobs))
    .join(
      "",
    )}</div><div class="section-heading"><h2>Recent jobs</h2><a href="#jobs">View all jobs →</a></div>${jobTable(jobs.slice(0, 5))}`;
}
function lineCard(w, jobs) {
  const runs = jobs.filter((j) => j.workflow === w.id);
  return `<article class="line-card"><div><div class="line-icon">⟐</div><h3>${escapeHTML(w.id)} <span class="version">v${w.version}</span></h3></div><p>${escapeHTML(w.description)}</p><div class="line-stages">${w.phases.map((p) => `<span class="stage-chip">${escapeHTML(p.id)}${p.gate ? " ◇" : ""}</span>`).join("")}</div><div class="line-footer"><span>${w.phases.length} phases · ${w.phases.filter((p) => p.gate).length} human gates</span><span>${runs.length} jobs</span></div></article>`;
}
function card(j) {
  return `<button class="case-card ${j.status === "awaiting_approval" ? "gated" : ""}" data-job="${j.id}"><span class="case-key"><span>${escapeHTML(j.issue.key)}</span><span class="subtle">${age(j.created_at)}</span></span><h3>${escapeHTML(j.issue.title)}</h3><div class="repo">${escapeHTML(j.repository)}</div><div class="line-name">${escapeHTML(j.workflow)}</div><div class="card-status">${j.status === "awaiting_approval" ? "◇" : "●"} ${escapeHTML(j.phase)} · ${escapeHTML(label(j.status))}</div><div class="card-foot"><span>${j.attempts} attempts</span><span>${short(j.id)} ↗</span></div></button>`;
}
function needsAttention(job) {
  return ["failed", "timed_out", "rejected", "cancelled"].includes(job.status) && !job.attention_dismissed;
}
function attentionAction(job) {
  if (!["failed", "timed_out", "rejected", "cancelled"].includes(job.status)) return "";
  const repo = typeof job.repository === "string" ? job.repository : job.repository.id;
  const scopes = access.operable_repositories || [];
  if (!scopes.includes("*") && !scopes.includes(repo)) return "";
  return `<button data-attention="${escapeHTML(job.id)}" data-dismissed="${!job.attention_dismissed}" title="Job history and receipts are kept">${job.attention_dismissed ? "Restore to Needs attention" : "Dismiss"}</button>`;
}
const attentionPending = new Set();
document.addEventListener("click", async e => {
  const button = e.target.closest("[data-attention]");
  if (!button || attentionPending.has(button.dataset.attention)) return;
  const id = button.dataset.attention, version = accessVersion;
  attentionPending.add(id);
  button.disabled = true;
  try {
    const response = await fetch(`/api/jobs/${encodeURIComponent(id)}/attention`, {method:"PUT", headers:{"Content-Type":"application/json", Authorization:`Bearer ${sessionStorage.getItem("factory-observer-token") || ""}`}, body:JSON.stringify({dismissed:button.dataset.dismissed === "true"})});
    const result = await response.json();
    if (!response.ok) throw Error(result.error || "Could not update Needs attention");
    if (version !== accessVersion) return;
    state.jobs = state.jobs.map(j => j.id === id ? {...j, attention_dismissed:result.attention_dismissed} : j);
    if (detailData?.job.id === id) detailData.job.attention_dismissed = result.attention_dismissed;
    render();
    if (detailData?.job.id === id) renderDetail();
    toast(result.attention_dismissed ? "Dismissed. Job history is kept in Jobs & receipts." : "Restored to Needs attention.");
    await refresh();
  } catch (error) {
    if (version === accessVersion) { $("error").textContent = error.message; $("error").hidden = false; }
  } finally { attentionPending.delete(id); button.disabled = false; }
});
function floor(jobs, events) {
  const current = cases(jobs),
    columns = [
      ["Intake", ["queued"]],
      ["Working", ["running"]],
      ["Awaiting human", ["awaiting_approval"]],
      ["Delivered", ["succeeded"]],
    ],
    attention = current.filter((j) =>
      needsAttention(j),
    );
  return `${stats(jobs)}<div class="floor-layout"><div><div class="board">${columns
    .map(([name, statuses]) => {
      const items = current.filter((j) => statuses.includes(j.status));
      return `<section class="column"><div class="column-header">${name}<span>${items.length}</span></div><div class="column-body">${items.map(card).join("") || '<div class="empty-column">No cases here</div>'}</div></section>`;
    })
    .join(
      "",
    )}</div><section class="attention"><h3>Needs attention · ${attention.length}</h3>${attention.length ? attention.map((j) => `<div class="attention-row">${jobButton(j, `${j.issue.key} · ${j.issue.title} · ${label(j.status)}`)}${attentionAction(j)}</div>`).join("") : '<span class="subtle">No cases need attention. Dismissed jobs remain in Jobs & receipts.</span>'}</section></div><aside class="rail"><section class="panel"><div class="panel-head"><h2>Production lines</h2><small>${Object.keys(state.catalog.workflows).length} defined</small></div>${Object.values(
    state.catalog.workflows,
  )
    .map(
      (w) =>
        `<div class="rail-row"><b>${escapeHTML(w.id)} <span class="version">v${w.version}</span></b><small>${w.phases.length} phases · ${w.phases.filter((p) => p.gate).length} gates · ${jobs.filter((j) => j.workflow === w.id).length} jobs</small></div>`,
    )
    .join(
      "",
    )}</section><section class="panel"><div class="panel-head"><h2>Live activity</h2><small>Recorded events</small></div>${milestones(events, 6)}</section></aside></div>`;
}
function jobTable(jobs) {
  if (!jobs.length)
    return empty(
      "No matching jobs",
      "Use Run workflow to start a job with operator access.",
    );
  return `<div class="table-wrap"><table><thead><tr><th>Case / job</th><th>Production line</th><th>Repository</th><th>State</th><th>Started</th><th>Agent time</th></tr></thead><tbody>${jobs.map((j) => `<tr><td>${jobButton(j, `${j.issue.key} · ${j.issue.title}`)}<small class="mono">${short(j.id)}${j.parent_id ? " · linked job" : ""}</small></td><td>${escapeHTML(j.workflow)}<small>${escapeHTML(j.phase)}</small></td><td>${escapeHTML(j.repository)}</td><td>${badge(j.status)}</td><td>${date(j.created_at)}</td><td class="mono">${(j.agent_ms / 60000).toFixed(2)} min</td></tr>`).join("")}</tbody></table></div>`;
}
function gates(jobs) {
  const pending = jobs.flatMap((j) =>
    j.gates.filter((g) => g.status === "pending").map((g) => ({ j, g })),
  );
  return `<div class="notice">Open Review evidence, read the artifacts, then approve or reject with maintainer access. Each decision applies to the exact artifact version shown.</div>${pending.length ? pending.map(({ j, g }) => `<article class="gate-card"><div class="gate-top"><div><h3>${escapeHTML(j.issue.key)} · ${escapeHTML(j.issue.title)}</h3><p>${escapeHTML(j.repository)} / ${escapeHTML(g.phase)}</p></div>${badge(g.status)}</div><p>Expires ${date(g.deadline)} · phase attempt <code>${g.attempt_id}</code></p><p>Artifact digest <code>${g.artifact_digest}</code></p><button data-job="${j.id}" data-detail-tab="gates">Review evidence ↗</button></article>`).join("") : empty("No decisions waiting", "Pending approvals appear here once a gated phase publishes its artifacts.")}`;
}
function lines() {
  const selected = $("workflow").value;
  return Object.values(state.catalog.workflows)
    .filter((w) => !selected || w.id === selected)
    .map(
      (w) =>
        `<details class="panel definition" open><summary><div><h3>⟐ ${escapeHTML(w.id)} <span class="version">v${w.version}</span></h3><p>${escapeHTML(w.description)}</p></div><span class="badge">${w.phases.length} phases</span></summary><div class="definition-body">${w.phases.map((p) => `<section class="phase-definition"><div><h4>${escapeHTML(p.id)}</h4><small>Timeout ${escapeHTML(p.timeout)}<br>Up to ${p.maxAttempts} attempts<br>${escapeHTML(p.permissions.join(", ") || "No repository permissions")}</small></div><div>${p.tasks.map((t) => `<div class="task-row ${t.uses === "agent.execute" ? "agent" : ""}"><span>${t.uses === "agent.execute" ? "✦" : "›"}</span><span>${escapeHTML(t.uses)}</span><small>${escapeHTML(t.with.prompt || t.with.name || t.with.profile || "")}</small></div>`).join("")}${p.gate ? `<div class="phase-note">◇ Human decision · ${escapeHTML(p.gate.channels.join(" / "))} · ${escapeHTML(p.gate.timeout)} deadline<br>Approval policy: ${escapeHTML(p.gate.approvers)}</div>` : ""}</div></section>`).join("")}<div class="panel-note">Worker ${escapeHTML(w.defaults.workerProfile)} · agent ${escapeHTML(w.defaults.agentProfile)} · revision pinned at job start${w.followUps.length ? "<br>Follow-up: " + w.followUps.map((f) => `${escapeHTML(f.workflow)} when ${escapeHTML(f.when)} · depth ≤ ${f.maxDepth}`).join(", ") : ""}</div></div></details>`,
    )
    .join("");
}
function platform() {
  const c = state.catalog;
  return `<div class="platform-grid"><article class="platform-card"><span class="eyebrow">CONTROL PLANE</span><h3>Factory server</h3><strong>Rust + PostgreSQL</strong><p>Execution state, gates, events, and pending dispatches are committed together. The coordinator resumes after a restart.</p></article><article class="platform-card"><span class="eyebrow">PHASE EXECUTION</span><h3>Disposable workers</h3><strong>${escapeHTML(c.executor === "ecs" ? "AWS ECS" : c.executor === "docker" ? "Docker" : "Local fixture processes")}</strong><p>One worker per phase attempt. Workers exit after reporting; waiting for a person uses no worker compute.</p></article><article class="platform-card"><span class="eyebrow">ARTIFACT STORAGE</span><h3>Durable outputs</h3><strong>${escapeHTML(c.storage)}</strong><p>Artifacts are addressed by SHA-256. Later phases receive only their declared inputs, with integrity verification.</p></article></div><div class="section-heading"><h2>Optional repository aliases</h2><small>Policy ${escapeHTML(c.policy_version)}</small></div><div class="table-wrap"><table><thead><tr><th>Repository</th><th>Provider</th><th>Base branch</th><th>Intake workflow</th></tr></thead><tbody>${c.repositories.map((r) => `<tr><td>${escapeHTML(r.id)}</td><td>${escapeHTML(r.provider)}</td><td>${escapeHTML(r.branch)}</td><td>${escapeHTML(r.workflow)}</td></tr>`).join("")}</tbody></table></div><div class="section-heading"><h2>Connected execution model</h2></div><div class="notice">External request → durable job → phase worker → artifact → human gate → fresh worker. Launch workflows from the portal and review gates with the appropriate workspace permissions.</div>`;
}
let connectorCatalog = [], editingConnector = null, connectorSaving = false;
let jiraFieldMappings = [], jiraFieldCatalog = [], jiraFieldRequest = 0;
function setupJiraFields(c) {
  jiraFieldRequest++;
  jiraFieldCatalog = [];
  jiraFieldMappings = JSON.parse(c.values.issue_fields || "[]");
  $("jira-field-editor").hidden = c.definition.kind !== "jira";
  $("jira-field-filter").value = "";
  $("jira-field-message").textContent = "";
  $("jira-field-preview").hidden = true;
  renderJiraMappings(); filterJiraFields();
}
function renderJiraMappings() {
  $("jira-field-rows").innerHTML = jiraFieldMappings.map((f,i) => `<div class="jira-mapping"><div class="jira-mapping-title"><strong>${i+1}. ${escapeHTML(jiraFieldCatalog.find(item => item.id === f.id)?.name || f.heading)}</strong><span class="badge">Included</span></div><small>Jira field ID: ${escapeHTML(f.id)}</small><label>Section heading in task details<input data-jira-heading="${i}" value="${escapeHTML(f.heading)}" maxlength="120"></label><div class="jira-mapping-actions"><button type="button" data-jira-move="${i}" data-direction="-1" ${i===0?"disabled":""}>Move up</button><button type="button" data-jira-move="${i}" data-direction="1" ${i===jiraFieldMappings.length-1?"disabled":""}>Move down</button><button type="button" data-jira-remove="${i}">Remove field</button></div></div>`).join("") || '<p>No additional fields selected. Add a field below to include more ticket context.</p>';
}
function filterJiraFields() {
  const query = $("jira-field-filter").value.toLowerCase();
  const matches = jiraFieldCatalog.filter(f => `${f.name} ${f.id}`.toLowerCase().includes(query));
  $("jira-field-choice").innerHTML = matches.map(f => {
    const added = jiraFieldMappings.some(m => m.id === f.id);
    return `<option value="${escapeHTML(f.id)}" ${added ? "disabled" : ""}>${escapeHTML(f.name)} (${escapeHTML(f.id)})${added ? " — Already included" : ""}</option>`;
  }).join("") || '<option value="">No matching fields. Load fields from Jira first.</option>';
}
$("jira-field-filter").oninput = filterJiraFields;
$("jira-add-field").onclick = () => {
  const field = jiraFieldCatalog.find(f => f.id === $("jira-field-choice").value);
  if (!field || jiraFieldMappings.length >= 20 || jiraFieldMappings.some(f => f.id===field.id)) return;
  jiraFieldMappings.push({id:field.id,heading:field.name});
  jiraFieldRequest++; $("jira-field-preview").hidden = true;
  renderJiraMappings(); filterJiraFields();
};
$("jira-field-rows").oninput = event => {
  const index = event.target.dataset.jiraHeading;
  if (index !== undefined && jiraFieldMappings[index]) jiraFieldMappings[index].heading = event.target.value;
};
$("jira-field-rows").onclick = event => {
  const remove = event.target.closest("[data-jira-remove]"), move = event.target.closest("[data-jira-move]");
  if (remove) jiraFieldMappings.splice(Number(remove.dataset.jiraRemove),1);
  else if (move) {
    const i = Number(move.dataset.jiraMove), j = i + Number(move.dataset.direction);
    if (j<0 || j>=jiraFieldMappings.length) return;
    [jiraFieldMappings[i],jiraFieldMappings[j]] = [jiraFieldMappings[j],jiraFieldMappings[i]];
  } else return;
  jiraFieldRequest++; $("jira-field-preview").hidden=true;
  renderJiraMappings(); filterJiraFields();
};
async function jiraEditorRequest(preview) {
  if (!editingConnector || connectorSaving) return;
  const editor = editingConnector, key = $("jira-preview-key").value.trim();
  if (preview && !key) { $("jira-field-message").textContent="Enter an issue key to preview."; return; }
  const version = ++jiraFieldRequest;
  $("jira-field-preview").hidden = true;
  $("jira-field-message").textContent = preview ? "Loading preview…" : "Loading Jira fields…";
  try {
    const path = preview ? `preview/${encodeURIComponent(key)}` : "fields";
    const response = await fetch(`/api/connectors/jira/${path}`, {method:"POST",headers:{"Content-Type":"application/json",Authorization:`Bearer ${sessionStorage.getItem("factory-observer-token") || ""}`},body:JSON.stringify(connectorPayload())});
    const result = await response.json();
    if (editingConnector!==editor || editor.version!==accessVersion || version!==jiraFieldRequest) return;
    if (!response.ok) throw Error(result.error || "Jira request failed");
    if (preview) {
      $("jira-field-preview").textContent = result.description || "(No task details)";
      $("jira-field-preview").hidden = false;
      $("jira-field-message").textContent = "Preview only; save to apply. " + ((result.field_statuses || []).map(f => `${f.heading}: ${f.status}`).join(" · ") || "Description only.");
    } else {
      jiraFieldCatalog = result.fields; renderJiraMappings(); filterJiraFields();
      $("jira-field-message").textContent = `${jiraFieldCatalog.length} fields available. Choose a field and click Add field.`;
    }
  } catch(error) {
    if (editingConnector===editor && editor.version===accessVersion && version===jiraFieldRequest) $("jira-field-message").textContent=error.message;
  }
}
$("jira-load-fields").onclick = () => jiraEditorRequest(false);
$("jira-preview").onclick = () => jiraEditorRequest(true);
$("connector-form").addEventListener("input", () => { jiraFieldRequest++; $("jira-field-preview").hidden=true; });
function connectors() {
  if (!access.manage_connectors) return '<div class="notice">Connector configuration requires a connector administrator account.</div><button data-connect>Connect administrator access</button>';
  return '<div class="notice">Credentials are encrypted on the server. Saved credentials are never returned to the browser. Changes apply to future requests and worker claims.</div><div class="platform-grid">' + connectorCatalog.map(c => `<article class="platform-card"><h3>${escapeHTML(c.definition.name)}</h3><p>${escapeHTML(c.definition.description)}</p><strong>${c.configured ? (c.enabled ? "Enabled" : "Disabled") : "Not configured in portal"}</strong><p>Access follows provider credentials${c.updated_by ? " · Updated by " + escapeHTML(c.updated_by) : ""}</p><button data-connector="${escapeHTML(c.definition.kind)}">Configure</button></article>`).join("") + '</div><p class="panel-note">Repositories are available according to provider credentials and workspace permissions. Existing environment settings are used only until a provider is saved here. Saving a disabled connection stops that provider from using the fallback.</p>';
}
function editConnector(kind) {
  const c = connectorCatalog.find(c => c.definition.kind === kind);
  if (!c || !access.manage_connectors) return;
  editingConnector = {kind, revision:c.revision, version:accessVersion};
  $("connector-title").textContent = c.definition.name;
  $("connector-enabled").checked = c.enabled;
  $("connector-fields").innerHTML = c.definition.fields.filter(f => f.key !== "issue_fields").map(f => `<label>${escapeHTML(f.label)}${f.required ? " (required when enabled)" : ""}<input data-field="${f.key}" type="${f.secret ? "password" : f.key.endsWith("url") ? "url" : "text"}" autocomplete="new-password" maxlength="8192" value="${f.secret ? "" : escapeHTML(c.values[f.key] || "")}" placeholder="${f.secret && c.secrets[f.key] ? "Saved — leave blank to keep" : ""}"></label>${f.secret && c.secrets[f.key] ? `<label class="connector-check"><input type="checkbox" data-clear="${f.key}">Clear saved credential</label>` : ""}`).join("");
  $("connector-error").hidden = true;
  $("connector-test-result").hidden = true;
  $("connector-editor").showModal();
  setupJiraFields(c);
}
document.addEventListener("click", e => { const b=e.target.closest("[data-connector]"); if(b)editConnector(b.dataset.connector); });
$("connector-close").onclick = () => { if (!connectorSaving) $("connector-editor").close(); };
$("connector-editor").addEventListener("cancel", e => { if(connectorSaving)e.preventDefault(); });
$("connector-editor").addEventListener("close", () => { $("connector-fields").innerHTML = ""; editingConnector = null; });
function connectorPayload() {
  const values = Object.fromEntries([...$("connector-fields").querySelectorAll("[data-field]")].map(el => [el.dataset.field, el.value.trim()]));
  if (editingConnector.kind === "jira") values.issue_fields = JSON.stringify(jiraFieldMappings);
  return {revision:editingConnector.revision, enabled:$("connector-enabled").checked, values, clear_secrets:[...$("connector-fields").querySelectorAll("[data-clear]:checked")].map(el => el.dataset.clear)};
}
$("connector-form").addEventListener("input", () => { $("connector-test-result").hidden = true; });
$("connector-test").onclick = async () => {
  if (connectorSaving || !editingConnector) return;
  const editor = editingConnector, payload = connectorPayload();
  connectorSaving = true;
  const controls = [...$("connector-form").querySelectorAll("input, button")];
  controls.forEach(el => el.disabled = true);
  $("connector-error").hidden = true;
  $("connector-test-result").hidden = false;
  $("connector-test-result").textContent = "Testing provider authentication…";
  try {
    const response = await fetch(`/api/connectors/${encodeURIComponent(editor.kind)}/test`, {method:"POST", headers:{"Content-Type":"application/json", Authorization:`Bearer ${sessionStorage.getItem("factory-observer-token") || ""}`}, body:JSON.stringify(payload)});
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || "Connection test failed");
    if (editor.version !== accessVersion || editingConnector !== editor) return;
    $("connector-test-result").innerHTML = `<strong>${result.ok ? "Authentication checks passed" : "Authentication check failed"}</strong><p>Tested ${escapeHTML(date(result.checked_at))}. Settings were not saved.</p>` + result.checks.map(c => `<p><strong>${escapeHTML(c.name)} · ${escapeHTML(label(c.status))}</strong><br>${escapeHTML(c.message)}</p>`).join("");
  } catch (error) {
    if (editor.version === accessVersion && editingConnector === editor) $("connector-test-result").textContent = error.message;
  } finally { connectorSaving = false; controls.forEach(el => el.disabled = false); }
};
$("connector-form").onsubmit = async e => {
  e.preventDefault(); if (connectorSaving || !editingConnector) return;
  const editor = editingConnector;
  const payload = connectorPayload();
  connectorSaving = true; $("connector-save").disabled = true; $("connector-test").disabled = true; $("connector-error").hidden = true;
  try {
    const response = await fetch(`/api/connectors/${encodeURIComponent(editor.kind)}`, {method:"PUT", headers:{"Content-Type":"application/json", Authorization:`Bearer ${sessionStorage.getItem("factory-observer-token") || ""}`}, body:JSON.stringify(payload)});
    if (!response.ok) throw new Error((await response.json()).error || "Save failed");
    if (editor.version !== accessVersion) return;
    $("connector-editor").close(); toast("Connector saved. Credentials have not been tested with the provider."); await refresh();
  } catch (error) { if (editor.version === accessVersion) { $("connector-error").textContent = error.message; $("connector-error").hidden = false; } }
  finally { connectorSaving = false; $("connector-save").disabled = false; $("connector-test").disabled = false; }
};

let pendingLaunch = null, launchBusy = false;
let jiraSearchTimer = null, jiraSearchVersion = 0, jiraMatches = [];
function clearJiraSearch() {
  clearTimeout(jiraSearchTimer);
  jiraSearchVersion++;
  jiraMatches = [];
  $("jira-search").hidden = true;
  $("jira-results").innerHTML = "";
}
async function searchJira(query, version, authVersion) {
  try {
    const response = await fetch(`/api/jira/issues?query=${encodeURIComponent(query)}`, {headers:{Authorization:`Bearer ${sessionStorage.getItem("factory-observer-token") || ""}`}});
    const result = await response.json();
    if (version !== jiraSearchVersion || authVersion !== accessVersion || launchBusy) return;
    if (!response.ok) throw Error(result.error || "Jira search failed");
    jiraMatches = result.issues || [];
    $("jira-search-status").textContent = jiraMatches.length ? "Select a ticket (up to 20 matches)." : "No matching tickets found.";
    $("jira-results").innerHTML = jiraMatches.map((issue, index) => `<button type="button" data-jira-index="${index}"><strong>${escapeHTML(issue.key)}</strong> ${escapeHTML(issue.summary)}</button>`).join("");
  } catch (error) {
    if (version === jiraSearchVersion && authVersion === accessVersion && !launchBusy) $("jira-search-status").textContent = error.message;
  }
}
function queueJiraSearch() {
  clearJiraSearch();
  if ($("launch-provider").value !== "jira" || launchBusy) return;
  $("jira-search").hidden = false;
  const query = $("launch-key").value.trim();
  if ([...query].length < 2) {
    $("jira-search-status").textContent = "Type at least 2 characters to search Jira by key or summary.";
    return;
  }
  $("jira-search-status").textContent = "Searching Jira…";
  const version = jiraSearchVersion, authVersion = accessVersion;
  jiraSearchTimer = setTimeout(() => searchJira(query, version, authVersion), 300);
}
$("launch-key").addEventListener("input", queueJiraSearch);
$("launch-provider").addEventListener("change", queueJiraSearch);
$("launch").addEventListener("close", clearJiraSearch);
function jiraDescriptionText(node) {
  if (typeof node === "string") return node;
  if (!node) return "";
  if (node.type === "text") return node.text || "";
  if (node.type === "hardBreak") return "\n";
  if (node.type === "mention") return node.attrs?.text || "";
  if (node.type === "inlineCard") return node.attrs?.url || "";
  const text = (node.content || []).map(jiraDescriptionText).join("");
  if (node.type === "listItem") return `- ${text.trim()}\n`;
  return text + (["paragraph", "heading", "codeBlock", "blockquote", "tableRow"].includes(node.type) ? "\n" : "");
}
$("jira-results").onclick = async event => {
  const button = event.target.closest("[data-jira-index]");
  if (!button || launchBusy) return;
  const issue = jiraMatches[Number(button.dataset.jiraIndex)];
  if (!issue) return;
  $("launch-key").value = issue.key;
  $("launch-task-title").value = issue.summary;
  clearJiraSearch();
  $("launch-key").focus();
  $("launch-body").value = "";
  $("jira-search").hidden = false;
  $("jira-search-status").textContent = "Loading ticket details…";
  const version = jiraSearchVersion, authVersion = accessVersion;
  try {
    const response = await fetch(`/api/jira/issues/${encodeURIComponent(issue.key)}`, {headers:{Authorization:`Bearer ${sessionStorage.getItem("factory-observer-token") || ""}`}});
    const result = await response.json();
    if (version !== jiraSearchVersion || authVersion !== accessVersion || launchBusy) return;
    if (!response.ok) throw Error(result.error || "Could not load ticket details");
    const description = jiraDescriptionText(result.description).trim();
    if (!$("launch-body").value) $("launch-body").value = description;
    $("jira-search-status").textContent = description ? "Ticket details loaded." : "This Jira ticket has no description.";
  } catch (error) {
    if (version === jiraSearchVersion && authVersion === accessVersion && !launchBusy) $("jira-search-status").textContent = error.message;
  }
};
function launchable(workflow) {
  return !workflow.phases.some(p => Object.values(p.inputs || {}).some(v => v.startsWith("parent.")));
}
function openLaunch() {
  if (!(access.operable_repositories || []).length || !state.catalog || launchBusy) return;
  const workflows = Object.values(state.catalog.workflows).filter(launchable);
  $("launch-workflow").innerHTML = workflows.map(w => `<option value="${escapeHTML(w.id)}">${escapeHTML(w.id)}</option>`).join("");
  $("launch-submit").disabled = !workflows.length;
  $("launch-error").hidden = true;
  $("launch").showModal();
  queueJiraSearch();
}
$("launch-button").onclick = openLaunch;
$("launch-close").onclick = () => { if (!launchBusy) $("launch").close(); };
$("launch").addEventListener("cancel", e => { if (launchBusy) e.preventDefault(); });
$("launch-form").onsubmit = async e => {
  e.preventDefault();
  if (launchBusy || !(access.operable_repositories || []).length) return;
  const version = accessVersion;
  const repository = $("launch-repository").value.trim();
  const workflow = $("launch-workflow").value;
  const provider = $("launch-provider").value;
  const key = $("launch-key").value.trim();
  const title = $("launch-task-title").value.trim();
  $("launch-error").hidden = true;
  const controls = [...$("launch-form").querySelectorAll("input, select, textarea, button")];
  try {
    if (!repository || !title || !state.catalog.workflows[workflow] || !launchable(state.catalog.workflows[workflow])) throw Error("Choose a workflow, repository, and task title.");
    if (provider !== "manual" && !key) throw Error("Enter the issue key or PR number.");
    if (provider.endsWith("_pr") && !/^[1-9][0-9]*$/.test(key)) throw Error("PR number must be a positive integer.");
    const scopes = access.operable_repositories;
    if (!scopes.includes("*") && !scopes.includes(repository)) throw Error("Your workspace token does not have operator access to this repository.");
    const draft = JSON.stringify({workflow, repository, issue:{provider, key, title, body:$("launch-body").value, url:null}});
    // Keep the exact payload and idempotency key after an uncertain response.
    if (!pendingLaunch || pendingLaunch.draft !== draft || pendingLaunch.version !== version) {
      const payload = JSON.parse(draft);
      const id = crypto.randomUUID();
      if (!payload.issue.key) payload.issue.key = `manual-${id}`;
      pendingLaunch = {draft, version, id, body:JSON.stringify(payload)};
    }
    launchBusy = true;
    controls.forEach(el => el.disabled = true);
    const response = await fetch("/api/jobs", {method:"POST", headers:{"Content-Type":"application/json", Authorization:`Bearer ${sessionStorage.getItem("factory-observer-token") || ""}`, "Idempotency-Key":pendingLaunch.id}, body:pendingLaunch.body});
    const result = await response.json();
    if (!response.ok) throw Error(result.error || "Workflow submission failed");
    if (version !== accessVersion) return;
    pendingLaunch = null;
    $("launch").close();
    $("launch-form").reset();
    toast(`Workflow started · ${short(result.id)}`);
    location.hash = "jobs";
    await refresh();
    await loadDetail(result.id);
  } catch (error) {
    if (version === accessVersion) {
      $("launch-error").textContent = error.message;
      $("launch-error").hidden = false;
    }
  } finally {
    launchBusy = false;
    controls.forEach(el => el.disabled = false);
  }
};

function render() {
  if (!state.loaded) return;
  const jobs = filteredJobs(),
    ids = new Set(jobs.map((j) => j.id)),
    events = state.events.filter((e) => ids.has(e.job_id)),
    p = page();
  const c = counts(state.jobs);
  $("nav-active").textContent = c.working;
  $("nav-gates").textContent = c.human;
  $("scope-label").textContent =
    `${cases(jobs).length} cases · ${jobs.length} jobs`;
  const views = {
    overview: () => overview(jobs, events),
    floor: () => floor(jobs, events),
    lines,
    jobs: () => jobTable(jobs),
    gates: () => gates(jobs),
    activity: () => `<div class="panel">${milestones(events, 100)}</div>`,
    platform,
    connectors,
  };
  setHTML($("content"), views[p]());
}
async function refresh() {
  if (busy) {
    refreshQueued = true;
    return;
  }
  const version = accessVersion;
  busy = true;
  $("refresh").disabled = true;
  try {
    const [catalog, jobs, events, permissions] = await Promise.all([
      api("/api/catalog"),
      api("/api/jobs"),
      api("/api/events"),
      api("/api/access"),
    ]);
    if (version !== accessVersion) return;
    access = permissions;
    const connections = permissions.manage_connectors ? (await api("/api/connectors")).connectors : [];
    if (version !== accessVersion) return;
    connectorCatalog = connections;
    state = { catalog, jobs: jobs.jobs, events: events.events, loaded: true };
    for (const [id, items, caption] of [
      [
        "repository",
        [...new Set([...catalog.repositories.map(r => r.id), ...jobs.jobs.map(j => j.repository)])].map(id => [id, id]),
        "All repositories",
      ],
      [
        "workflow",
        Object.values(catalog.workflows).map((w) => [w.id, w.id]),
        "All production lines",
      ],
    ]) {
      const selected = $(id).value;
      $(id).innerHTML =
        `<option value="">${caption}</option>` +
        items
          .map(
            ([v, l]) =>
              `<option value="${escapeHTML(v)}">${escapeHTML(l)}</option>`,
          )
          .join("");
      $(id).value = selected;
    }
    $("error").hidden = true;
    $("connection").className = "connection online";
    $("connection").innerHTML = "<i></i>Control plane connected";
    $("last-sync").textContent =
      `Last synced ${new Date().toLocaleTimeString()} · latest 500 jobs`;
    $("access-label").textContent =
      access.subject || "Local observation enabled";
    $("launch-button").disabled = !(access.operable_repositories || []).length;
    $("access-mode").textContent = access.approvable_repositories.length
      ? "◉ MAINTAINER ACCESS"
      : (access.operable_repositories || []).length ? "◉ OPERATOR ACCESS" : "◉ READ-ONLY";
    render();
    if (selectedJob && $("detail").open) await loadDetail(selectedJob, false);
  } catch (error) {
    if (version !== accessVersion) return;
    $("error").hidden = false;
    $("error").textContent =
      `${error.message}. ${state.loaded ? "Showing the last successful snapshot." : "Start factory-server to load durable operations data. Use Observer access if authentication is required."}`;
    $("connection").className = "connection offline";
    $("connection").innerHTML = "<i></i>Connection unavailable";
    if (!state.loaded)
      setHTML(
        $("content"),
        empty(
          "Waiting for the control plane",
          "This portal displays recorded execution data when the server is available.",
        ),
      );
  } finally {
    busy = false;
    $("refresh").disabled = false;
    if (refreshQueued) {
      refreshQueued = false;
      refresh();
    }
  }
}
async function loadDetail(id, open = true) {
  const version = accessVersion;
  try {
    const data = await api(`/api/jobs/${encodeURIComponent(id)}`);
    if (version !== accessVersion) return;
    if (!open && selectedJob !== id) return;
    selectedJob = id;
    detailData = data;
    if (open && !$("detail").open) $("detail").showModal();
    renderDetail();
  } catch (e) {
    toast(e.message);
  }
}
function attemptStatus(a) {
  if (a.status === "timed_out")
    return a.started_at ? "execution_timed_out" : "launch_timed_out";
  return a.status;
}
function attemptError(a) {
  // Older receipts used one generic deadline message. Preserve the receipt,
  // but explain its timeout using the recorded worker claim timestamp.
  if (a.status === "timed_out" && a.error === "Attempt deadline elapsed")
    return a.started_at
      ? "Phase execution timed out: the worker exceeded the phase deadline"
      : "Worker launch timed out: no worker claimed the attempt before the launch deadline";
  return a.error;
}
function renderDetail() {
  if (!detailData) return;
  const j = detailData.job;
  $("detail-key").textContent = `${j.issue.key} / ${short(j.id)}`;
  $("detail-title").textContent = j.issue.title;
  $("detail-meta").textContent =
    `${j.repository.id} · ${j.workflow} · ${label(j.status)}`;
  setHTML(
    $("case-history"),
    state.jobs
      .filter((x) => x.case_id === j.case_id)
      .slice()
      .reverse()
      .map(
        (x) =>
          `<button data-job="${x.id}" class="${x.id === j.id ? "active" : ""}">${short(x.id)} · ${escapeHTML(label(x.status))}</button>`,
      )
      .join(""),
  );
  document.querySelectorAll("[data-tab]").forEach((b) => {
    b.setAttribute("aria-selected", String(b.dataset.tab === detailTab));
    b.tabIndex = b.dataset.tab === detailTab ? 0 : -1;
  });
  let html = "";
  if (detailTab === "timeline") {
    html = j.attempts
      .map(
        (a) =>
          `<section class="attempt"><div class="attempt-head"><div><h3>${escapeHTML(a.phase)} · attempt ${a.number}</h3><small>${a.id} · ${date(a.started_at || a.created_at)}</small></div>${badge(attemptStatus(a))}</div>${attemptError(a) ? `<div class="notice">${escapeHTML(attemptError(a))}</div>` : ""}<p class="panel-note">Queued ${date(a.created_at)} · ${a.started_at ? "Worker started " + date(a.started_at) : "Worker never started"}<br>${a.started_at ? "Execution" : "Launch"} deadline ${date(a.deadline)}</p>${a.result ? a.result.tasks.map((t) => `<div class="task-row ${t.task === "agent.execute" ? "agent" : ""}"><span>${t.status === "succeeded" ? "✓" : "×"}</span><span>${escapeHTML(t.task)}<small style="display:block;margin-top:6px">${escapeHTML(t.summary)}</small></span><small>${(t.duration_ms / 1000).toFixed(1)}s</small></div>`).join("") : '<div class="panel-note">' + (a.status === "queued" ? "Waiting for worker dispatch." : a.status === "running" ? "Worker is executing. Results are recorded when the phase reports." : "No task completion report.") + "</div>"}${Object.values(a.artifacts).map(x => artifactRow(x, j.id)).join("")}</section>`,
      )
      .join("");
  } else if (detailTab === "gates") {
    html = j.gates.length
      ? j.gates
          .map(
            (g) =>
              `<section class="gate-card"><div class="gate-top"><h3>${escapeHTML(g.phase)} gate</h3>${badge(g.status)}</div><p>Gate <code>${g.id}</code><br>Attempt <code>${g.attempt_id}</code><br>Artifact digest <code>${g.artifact_digest}</code><br>Deadline ${date(g.deadline)}${g.decided_by ? `<br>Decision by ${escapeHTML(g.decided_by)} via ${escapeHTML(g.channel)} · ${date(g.decided_at)}` : ""}</p>${Object.values(
                j.attempts.find((a) => a.id === g.attempt_id)?.artifacts || {},
              )
                .map(x => artifactRow(x, j.id))
                .join("")}${gateActions(j, g)}</section>`,
          )
          .join("")
      : empty("No gates recorded", "This job has not reached a human gate.");
  } else {
    const worker =
      j.snapshot.workers[
        j.snapshot.workflows[j.workflow].defaults.workerProfile
      ];
    html = `<div class="receipt-tools"><span>Recorded execution provenance</span><button data-receipt="${j.id}">Download JSON receipt ↓</button></div><article class="receipt"><h3>Job execution receipt</h3><dl>${[
      ["Job", j.id],
      ["Case", j.case_id],
      ["Parent job", j.parent_id || "—"],
      ["State", label(j.status)],
      ["Requested by", j.requested_by],
      [
        "Workflow",
        `${j.workflow} · v${j.snapshot.workflows[j.workflow].version}`,
      ],
      ["Repository revision", j.repository.revision],
      ["Definition hash", j.snapshot.definition_hash],
      ["Worker image", worker.image],
      ["Policy", j.snapshot.policy_version],
      ["Platform", j.snapshot.platform_version],
      ["Platform revision", j.snapshot.platform_revision || "unversioned"],
      ["Started", date(j.created_at)],
      ["Finished", date(j.finished_at)],
    ]
      .map(
        ([k, v]) =>
          `<dt>${escapeHTML(k)}</dt><dd class="mono">${escapeHTML(v)}</dd>`,
      )
      .join(
        "",
      )}</dl><h4>AGENT EXECUTION</h4>${j.attempts.flatMap((a) => (a.result?.agent_runs || []).map((r) => `<p>${escapeHTML(a.phase)} · ${escapeHTML(r.harness)} ${escapeHTML(r.harness_version)} · ${escapeHTML(r.model)} · ${escapeHTML(r.status)}<br>${r.prompt_tokens} input tokens · ${r.completion_tokens} output tokens</p>`)).join("") || "<p>No model execution recorded (fixture or legacy command).</p>"}<h4>PROMPT PROVENANCE</h4>${Object.entries(
      j.snapshot.prompts,
    )
      .map(([name, p]) => `<p>${escapeHTML(name)} · SHA-256 ${p.sha256}</p>`)
      .join(
        "",
      )}<h4>EXECUTION</h4><p>${j.attempts.length} attempts · ${j.gates.length} human gates · ${j.attempts.reduce((n, a) => n + Object.keys(a.artifacts).length, 0)} published artifacts</p></article>`;
  }
  setHTML($("detail-body"), attentionAction(j) + html);
}
function canApproveRepository(id) {
  if (access.approvable_repositories.includes(id)) return true;
  if (state.catalog.repositories.some(r => r.id === id)) return false;
  const scopes = access.dynamic_approvable_repositories || [];
  return scopes.includes("*") || scopes.includes(id);
}
function gateActions(job, gate) {
  if (gate.status !== "pending") return "";
  if (new Date(gate.deadline).getTime() <= Date.now())
    return '<p class="notice">This gate has expired. Refresh to see its final status.</p>';
  const definition = job.snapshot.workflows[job.workflow].phases.find(
    (p) => p.id === gate.phase,
  )?.gate;
  if (!definition?.channels.includes("api"))
    return '<p class="notice">This gate requires a decision through its configured external channel.</p>';
  if (!access.subject)
    return '<p class="notice">Connect with a maintainer token to decide after reviewing the evidence.</p><button data-connect>Connect maintainer access</button>';
  if (!canApproveRepository(job.repository.id))
    return '<p class="notice">Your identity does not have maintainer approval access for this repository.</p>';
  return `<div class="actions"><button class="primary" data-decide="${gate.id}" data-approve="true">Approve</button><button data-decide="${gate.id}" data-approve="false">Reject</button></div>`;
}
function beginDecision(gateId, approve) {
  const job = detailData?.job,
    gate = job?.gates.find((g) => g.id === gateId);
  if (
    !gate ||
    gate.status !== "pending" ||
    !canApproveRepository(job.repository.id)
  )
    return;
  const key = `${accessVersion}:${job.id}:${gate.id}:${gate.artifact_digest}:${approve}`;
  if (!decisionIds.has(key)) decisionIds.set(key, crypto.randomUUID());
  pendingDecision = {
    jobId: job.id,
    version: accessVersion,
    body: {
      event_id: decisionIds.get(key),
      gate_id: gate.id,
      artifact_digest: gate.artifact_digest,
      approve,
    },
  };
  $("decision-title").textContent = approve
    ? "Approve this artifact version?"
    : "Reject this artifact version?";
  $("decision-summary").textContent =
    `${job.issue.key} · ${gate.phase} · attempt ${short(gate.attempt_id)} · as ${access.subject}`;
  $("decision-consequence").textContent = approve
    ? "Approval allows the workflow to advance. Subsequent phases may modify the repository and publish a pull request."
    : "Rejection stops this job. Its evidence and decision remain in the receipt.";
  $("decision-digest").textContent = gate.artifact_digest;
  $("decision-reviewed").checked = false;
  $("decision-error").hidden = true;
  $("submit-decision").textContent = approve
    ? "Confirm approval"
    : "Confirm rejection";
  $("decision").showModal();
}
$("decision-form").onsubmit = async (e) => {
  e.preventDefault();
  if (decisionBusy || !pendingDecision || !$("decision-reviewed").checked)
    return;
  const decision = pendingDecision;
  decisionBusy = true;
  $("submit-decision").disabled = true;
  $("decision-error").hidden = true;
  try {
    if (decision.version !== accessVersion)
      throw new Error("Access changed. Reopen the evidence before deciding.");
    const token = sessionStorage.getItem("factory-observer-token");
    if (!token) throw new Error("Connect a maintainer token first.");
    const response = await fetch(
      `/api/jobs/${encodeURIComponent(decision.jobId)}/decisions`,
      {
        method: "POST",
        headers: {
          Authorization: `Bearer ${token}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify(decision.body),
      },
    );
    const result = await response.json();
    if (!response.ok)
      throw new Error(result.error || `Decision failed (${response.status})`);
    $("decision").close();
    toast(
      decision.body.approve
        ? "Approval recorded. The workflow can advance."
        : "Rejection recorded. Job stopped.",
    );
    await refresh();
  } catch (error) {
    $("decision-error").textContent =
      `${error.message} Refresh the evidence if the gate changed. If the response was lost, retrying this same decision is safe.`;
    $("decision-error").hidden = false;
  } finally {
    decisionBusy = false;
    $("submit-decision").disabled = false;
  }
};
for (const id of ["close-decision", "cancel-decision"])
  $(id).onclick = () => {
    if (!decisionBusy) $("decision").close();
  };
$("decision").addEventListener("cancel", (e) => {
  if (decisionBusy) e.preventDefault();
});
$("decision").addEventListener("close", () => {
  pendingDecision = null;
});
function artifactRow(a, jobId) {
  return `<div class="artifact-link"><div>${escapeHTML(a.name)} · ${a.size.toLocaleString()} bytes<br><code>SHA-256 ${a.sha256}</code></div><div><a href="${reportLink(jobId, a.id)}">View report ↗</a> <button data-artifact="${a.id}" data-name="${escapeHTML(a.name)}">Download ↓</button></div></div>`;
}
async function download(path, name, json = false) {
  try {
    const token = sessionStorage.getItem("factory-observer-token"),
      res = await fetch(path, {
        headers: token ? { Authorization: `Bearer ${token}` } : {},
      });
    if (!res.ok) throw new Error(`Download failed (${res.status})`);
    const blob = json
      ? new Blob([JSON.stringify(await res.json(), null, 2)], {
          type: "application/json",
        })
      : await res.blob();
    const url = URL.createObjectURL(blob),
      a = document.createElement("a");
    a.href = url;
    a.download = name;
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  } catch (e) {
    toast(e.message);
  }
}
function toast(message) {
  $("toast").textContent = message;
  $("toast").hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => ($("toast").hidden = true), 4000);
}
document.addEventListener("click", (e) => {
  const el = e.target.closest(
    "[data-job],[data-tab],[data-artifact],[data-receipt],[data-decide],[data-connect]",
  );
  if (!el) return;
  if (el.hasAttribute("data-connect")) $("credentials").showModal();
  if (el.dataset.decide)
    beginDecision(el.dataset.decide, el.dataset.approve === "true");
  if (el.dataset.job) {
    detailTab = el.dataset.detailTab || "timeline";
    loadDetail(el.dataset.job);
  }
  if (el.dataset.tab) {
    detailTab = el.dataset.tab;
    renderDetail();
  }
  if (el.dataset.artifact)
    download(`/api/artifacts/${el.dataset.artifact}`, `${el.dataset.name}.md`);
  if (el.dataset.receipt)
    download(
      `/api/jobs/${el.dataset.receipt}/receipt`,
      `factory-receipt-${short(el.dataset.receipt)}.json`,
      true,
    );
});
for (const id of ["repository", "workflow"])
  $(id).addEventListener("change", render);
$("search").addEventListener("input", render);
$("refresh").addEventListener("click", refresh);
$("close-detail").onclick = () => $("detail").close();
$("close-report").onclick = () => { $("report").close(); if (location.hash.startsWith("#report/")) location.hash = "jobs"; };
window.addEventListener("hashchange", openReportHash);
openReportHash();
$("detail").addEventListener("close", () => {
  selectedJob = null;
  detailData = null;
});
$("connection-button").onclick = () => $("credentials").showModal();
$("close-credentials").onclick = () => $("credentials").close();
$("credential-form").onsubmit = (e) => {
  e.preventDefault();
  sessionStorage.setItem("factory-observer-token", $("api-token").value.trim());
  $("api-token").value = "";
  $("credentials").close();
  resetAccess();
  if (location.hash.startsWith("#report/")) openReportHash();
};
$("clear-token").onclick = () => {
  sessionStorage.removeItem("factory-observer-token");
  $("credentials").close();
  resetAccess();
};
$("theme").onclick = () => {
  document.documentElement.dataset.theme =
    document.documentElement.dataset.theme === "dark" ? "light" : "dark";
  localStorage.setItem("factory-theme", document.documentElement.dataset.theme);
};
document.documentElement.dataset.theme =
  localStorage.getItem("factory-theme") || "dark";
document.querySelector(".tabs").addEventListener("keydown", (e) => {
  if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key)) return;
  e.preventDefault();
  const tabs = [...document.querySelectorAll("[data-tab]")];
  const current = tabs.indexOf(document.activeElement);
  const next =
    e.key === "Home"
      ? 0
      : e.key === "End"
        ? tabs.length - 1
        : (current + (e.key === "ArrowRight" ? 1 : -1) + tabs.length) %
          tabs.length;
  detailTab = tabs[next].dataset.tab;
  renderDetail();
  tabs[next].focus();
});
addEventListener("hashchange", route);
route();
refresh();
setInterval(() => {
  if (!document.hidden) refresh();
}, 5000);
setInterval(
  () =>
    ($("clock").textContent = new Date().toISOString().slice(11, 19) + " UTC"),
  1000,
);
