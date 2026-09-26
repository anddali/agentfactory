# Factories — operating guide

For the current installation in `C:\Users\andri\Projects\factory`. Configuration and runtime checked **18 September 2026**. See [TECHNICAL.md](TECHNICAL.md) for architecture, API reference, and connector details.

## Current state

| Component | Status |
| --- | --- |
| Portal | [http://127.0.0.1:8787](http://127.0.0.1:8787). Local observation enabled; authenticated maintainers can decide gates. |
| Control plane | Rust server in Docker; PostgreSQL 17 persists jobs and approvals. |
| Artifacts | Filesystem storage in a persistent Docker volume. |
| Workers | Disposable Docker containers; a fresh worker for each phase attempt. |
| Agent | OpenHands SDK **1.49.2**, **`openai/gpt-5.6-luna`**, API key in `.env`. |
| Live OpenAI check | Passed: tools created a file and its contents were verified. This was an isolated check, not a repository delivery. |
| Registered repository | **`local-demo` only**, a fixture without a Git repository. |
| Demo | Real scheduling, gates, and artifacts, but fixture text and **no model calls**. |
| Real repository workflows | Implemented; need repository registration, Git credentials, and repository-specific build/test tools. |
| External connectors / AWS | Adapters exist; not configured or acceptance-tested for this installation. |
| Ollama | Reachable from Docker, but Llama/Gemma harness checks failed to produce the output. Not the active provider. |

**The portal supports gate approval/rejection for authenticated maintainers.** Submit jobs through the API commands below. Existing demo cards do not represent work performed by OpenAI.

## 1. Start and stop

Start Docker Desktop. Use **PowerShell 7** and run commands from the project folder:

```powershell
Set-Location C:\Users\andri\Projects\factory
docker compose up -d db server
docker compose ps
Invoke-RestMethod http://127.0.0.1:8787/api/health
```

Health should report `status: ok` and `executor: docker`. Open [the factory floor](http://127.0.0.1:8787/#floor). PostgreSQL is available on host port `54329`; workers contact the server on internal port `8080`.

The images and `.env` already exist here. To build from scratch:

```powershell
./scripts/setup.ps1
docker compose build server worker-image openhands-image
docker compose up -d db server
```

`setup.ps1` only creates credentials when `.env` is missing. A fresh setup also needs a model API key before live use.

```powershell
docker compose logs --tail 100 server
docker compose logs --tail 50 db
docker compose restart server   # same image and environment
docker compose stop             # preserve database and artifact volumes
```

Cancel or finish active jobs before planned downtime. Pending gates survive restart; running workers can lose their heartbeat lease during a long outage and consume retries. Keep `.env`, especially `FACTORY_WORKER_SECRET`, across restarts.

**Do not use `docker compose down -v` for normal shutdown: it removes persisted data.** Database and artifact volumes must be retained together. Automated backup/restore is not implemented.

## 2. Use the portal

| Screen | Purpose |
| --- | --- |
| Overview | Counts and recent activity. |
| Factory floor | Cases grouped by progress and attention needed. |
| Production lines | Phase definitions, tasks, and gates. |
| Jobs & receipts | Inspect individual jobs and attempts. |
| Human gates | Find work awaiting a maintainer decision. |
| Activity | Recorded execution events. |
| Platform | Executor, storage, and policy details. |

Open a job to view **Timeline**, **Gates**, and **Receipt**. Download outputs from Timeline/Gates. Receipt records pinned definitions, prompts, image digests, and any harness/model execution and token usage.

The portal refreshes roughly every five seconds and shows the latest 500 jobs/events. Search and repository/workflow selectors filter the view. No running worker at a pending gate is normal. Fixture/older receipts can have no recorded model execution.

## 3. Run the demo

```powershell
./scripts/smoke.ps1
```

This creates a new demo job, checks duplicate submission, **automatically approves both fixture gates**, checks decision replay, and verifies the final artifact hash. No OpenAI calls or Git changes occur.

To review manually instead:

```powershell
./scripts/smoke.ps1 -LeavePending
```

Copy its job ID. Review research in the portal, then approve using section 5. A fresh plan worker runs next. Review and approve the plan separately to start delivery. Each gate expires after 72 hours.

`./scripts/restart-check.ps1` creates another fixture job, restarts the server at a pending gate, verifies persistence, and leaves that gate pending. Test records remain visible in the portal.

## 4. Authenticate and submit

Load the operator identity without displaying its token:

```powershell
. ./scripts/env.ps1
$baseUrl = 'http://127.0.0.1:8787'
$operator = @($env:FACTORY_IDENTITIES | ConvertFrom-Json) |
    Where-Object subject -eq 'local-maintainer' | Select-Object -First 1
if (-not $operator) { throw 'local-maintainer identity is missing.' }
$headers = @{ Authorization = "Bearer $($operator.token)" }
```

`LLM_API_KEY` is for model calls; `FACTORY_IDENTITIES` authorizes people operating Factories. Do not paste these credentials into prompts, issues, or receipts. The current maintainer has access to `local-demo`.

Submit a manual fixture job:

```powershell
$requestKey = "manual-$([Guid]::NewGuid())"
$submitHeaders = $headers.Clone()
$submitHeaders['Idempotency-Key'] = $requestKey
$request = @{
    workflow = 'demo'
    repository = 'local-demo'
    issue = @{
        provider = 'fixture'
        key = "MANUAL-$([Guid]::NewGuid())"
        title = 'My first factory run'
        body = 'Exercise research, plan, and delivery with manual approvals.'
    }
} | ConvertTo-Json -Depth 5
$submitted = Invoke-RestMethod "$baseUrl/api/jobs" -Method Post `
    -Headers $submitHeaders -ContentType application/json -Body $request
$jobId = $submitted.id
$jobId
```

Retry an uncertain submission with the **same key and body** to retrieve the same job. A new key creates a new job; an existing key with different content is rejected. Reusing the same issue key/provider in a repository groups runs into a case.

For an existing job, set `$jobId = 'PASTE-JOB-UUID'`. Inspect it with:

```powershell
$detail = Invoke-RestMethod "$baseUrl/api/jobs/$jobId" -Headers $headers
$detail.job | Select-Object id, status, phase_index
$detail.job.attempts | Select-Object id, phase, number, status, error
```

## Approve or reject in the portal

1. Click **Workspace access** at the bottom of the sidebar.
2. Enter the Factories token for `local-maintainer` from `FACTORY_IDENTITIES` in `.env` and click **Connect**. This is not `LLM_API_KEY`. The token stays in this tab's session; **Clear token** signs out.
3. Open **Human gates → Review evidence**. Download and read the artifacts under the **Gates** tab.
4. Click **Approve** or **Reject**. Check the phase/attempt and digest in the confirmation, acknowledge that you reviewed the artifacts, and confirm.
5. The decision is recorded and the view refreshes. Approval advances the workflow; rejection stops the job.

Anonymous viewers see **Connect maintainer access**. Authenticated observers without repository approval rights cannot decide. Expired/decided gates and gates restricted to Slack have no portal decision controls. The server checks authority, deadline and exact artifact identity again when a decision arrives. An uncertain request can be retried with the same decision identity.

To copy the local maintainer token without printing it, run this yourself in PowerShell, then paste it into Workspace access:

```powershell
. ./scripts/env.ps1
(@($env:FACTORY_IDENTITIES | ConvertFrom-Json) |
    Where-Object subject -eq 'local-maintainer' |
    Select-Object -First 1).token | Set-Clipboard
```

## 5. Review and decide

Continue in the authenticated session above:

```powershell
$detail = Invoke-RestMethod "$baseUrl/api/jobs/$jobId" -Headers $headers
$gate = @($detail.job.gates | Where-Object status -eq 'pending')[0]
if (-not $gate) { throw 'No pending gate.' }
$attempt = $detail.job.attempts | Where-Object id -eq $gate.attempt_id
$attempt.artifacts.PSObject.Properties.Value |
    Select-Object id, name, sha256, size
```

Download and read the artifacts from that exact attempt in the portal, or use an artifact ID:

```powershell
$artifactId = 'PASTE-ARTIFACT-UUID'
Invoke-WebRequest "$baseUrl/api/artifacts/$artifactId" -Headers $headers `
    -OutFile "$env:TEMP/factory-review-$artifactId.md"
```

The gate's **`artifact_digest` identifies the artifact set**. It is not interchangeable with an individual file's SHA-256.

Approve after review:

```powershell
$decision = @{
    event_id = "manual-decision-$([Guid]::NewGuid())"
    gate_id = $gate.id
    artifact_digest = $gate.artifact_digest
    approve = $true
} | ConvertTo-Json
Invoke-RestMethod "$baseUrl/api/jobs/$jobId/decisions" -Method Post `
    -Headers $headers -ContentType application/json -Body $decision
```

Use `approve = $false` to reject instead; rejection stops the job. Retry a network failure with the same `$decision` payload/event ID. A conflicting replay, expired gate, or wrong digest is rejected. Fetch the job again before reviewing and deciding the next gate.

Cancel active work:

```powershell
Invoke-RestMethod "$baseUrl/api/jobs/$jobId/cancel" -Method Post -Headers $headers
```

Cancellation fences the current attempt and schedules worker cleanup. It does not undo commits or PRs already published. Automatic phase retries are bounded. There is **no manual retry/resume endpoint**; fix the cause and submit a new job with a new request key.

Export a receipt:

```powershell
Invoke-WebRequest "$baseUrl/api/jobs/$jobId/receipt" -Headers $headers `
    -OutFile "$env:TEMP/factory-receipt-$jobId.json"
```

## 6. Model configuration and applying changes

The active profile in [config/platform.yaml](config/platform.yaml) uses:

```yaml
backend: openhands
env_keys: [LLM_API_KEY]
openhands:
  sdk_version: 1.49.2
  model: openai/gpt-5.6-luna
  api_key_env: LLM_API_KEY
  base_url: null
  api_mode: auto
  max_iterations: 40
  max_output_tokens: 16000
  timeout_seconds: 1200
  tools: [terminal, file_editor]
```

`max_output_tokens` limits each response, **not total spend**. The 20-minute harness limit applies even when the phase permits longer. `reasoning_effort` is optional and defaults to null. There is no automatic model fallback. Additional profiles can be selected per phase with `agentProfile: name`; otherwise the workflow default applies.

Keep keys in ignored `.env`, and model/limits in YAML.

| Change | Apply |
| --- | --- |
| `.env` or Compose environment | `docker compose up -d --force-recreate server` |
| Model/profile, web (workflow/prompt changes use the portal) | `docker compose build server`, then `docker compose up -d server` |
| OpenHands adapter or dependencies | `docker compose build openhands-image` |
| Worker code/toolchain | Rebuild affected worker images; rebuild server too for shared Rust changes. |

Use a fresh shell or reload `./scripts/env.ps1` after editing `.env`; existing shell variables can override Compose values. A plain restart does not load a new image or changed environment.

Existing jobs keep pinned profiles, prompts, and image digests. New jobs use updated definitions. Keep old images while unfinished jobs reference them. Credentials are resolved at worker claim time and are not saved in job snapshots.

### Repeat the isolated live model check

This **uses API credits**. It operates only on a temporary file, creates no portal job, and does not access a repository. It caps execution at four iterations, 2,048 output tokens per response, and 120 seconds, or lower configured limits.

```powershell
. ./scripts/env.ps1
$repoPath = (Get-Location).Path
@{ credential = $env:LLM_API_KEY } | ConvertTo-Json -Compress |
    docker run --rm -i --read-only --cap-drop ALL --security-opt no-new-privileges `
    --tmpfs /work:rw,uid=10001,gid=10001,size=536870912 --tmpfs /tmp:rw,size=134217728 `
    --mount "type=bind,source=$repoPath/config/platform.yaml,target=/config/platform.yaml,readonly" `
    --mount "type=bind,source=$repoPath/harnesses/openhands/live_check.py,target=/tests/live_check.py,readonly" `
    --entrypoint python factory-worker:openhands /tests/live_check.py
```

Success: `status: finished`, `output_verified: true`, exit code 0. The previous successful check reported 17,690 input tokens and 263 output tokens; future usage varies. The credential travels over stdin and is not printed.

## 7. Connect a real repository

This setup is still required for issue-to-PR operation. **Do not run `research-plan` against `local-demo`: checkout cannot operate on a fixture URL.**

1. Add a real repository under `repositories` in `config/platform.yaml`, replacing placeholders:

   ```yaml
   example-service:
     provider: github
     url: https://github.com/OWNER/REPO.git
     api_url: https://api.github.com/repos/OWNER/REPO
     branch: main
     workflow: research-plan
     maintainers: [local-maintainer]
     read_token_env: EXAMPLE_REPO_READ_TOKEN
     write_token_env: EXAMPLE_REPO_WRITE_TOKEN
   ```

2. Add Git credentials to `.env` with the necessary repository read/branch/PR access. Wire them into `services.server.environment` in `compose.yaml`, for example `EXAMPLE_REPO_READ_TOKEN: ${EXAMPLE_REPO_READ_TOKEN}` and the corresponding write-token entry. Arbitrary `.env` variables are not automatically passed into the server.

3. Add `example-service` to the maintainer identity's `repositories` array in `FACTORY_IDENTITIES`. Preserve its token and `observer`, `operator`, and `approver` roles. Both identity access and repository `maintainers` must permit approval.

4. Extend the `openhands-worker` Docker stage with your repository's language/build tools. **The supplied image has Python and Git, but no Node/npm or Rust compiler.** Replace the current npm `repository-tests` placeholder with the correct command/arguments, install those tools, and check the repository builds in the image.

5. For API-only decisions, change research and plan gates in `workflows/research-plan.yaml` to `channels: [api]`. They currently list `[slack, api]`. Unconfigured Slack produces notification delivery errors/retries. For Slack operation, see [connector setup](TECHNICAL.md#connector-setup).

6. Rebuild server and OpenHands images, recreate the server, and check health/logs.

Use section 4's submission command with `workflow = 'research-plan'`, `repository = 'example-service'`, and the issue's provider/key/title/body. The workflow checks out a pinned revision, researches, waits for approval, plans, waits again, then implements, validates, pushes a branch, and opens a PR. **Approving the plan permits that implementation sequence.** It does not automatically merge.

PR review requires only a PR link. Choose **pr-review** in **Run workflow**, paste a GitHub or Azure DevOps PR URL, and optionally add a Jira key/link, GitHub issue link, or Azure work item link. The platform fetches the real title/description and optional ticket (including configured Jira fields or Azure acceptance criteria). The equivalent API is `POST /api/pr-reviews` with `{"pr_url":"https://github.com/owner/repo/pull/123","ticket":"TEAM-42"}`, an operator bearer token and `Idempotency-Key`; `ticket` is optional and `workflow` defaults to `pr-review`. The older structured submission API remains supported and also hydrates PR metadata.

Each review collects fresh general comments, submitted reviews and inline discussion, including replies and resolved/outdated thread metadata, then computes the entire merge-base-to-pinned-head diff. Context, diff, findings JSON and the readable review are preserved as portal artifacts. GitHub findings are published as a commit-linked review, with inline comments where the location belongs to the diff; Azure findings are published as a PR discussion thread. Existing concerns stay in the fix input and summary without duplicate inline comments when the report references their comment ID. Provider errors, oversized context and pagination limits fail explicitly rather than silently dropping discussion. GitHub thread resolution requires GraphQL read access, and publication requires PR write access (`pull_request.comment` platform permission). See the [GitHub review API](https://docs.github.com/en/rest/pulls/reviews), [GitHub thread schema](https://docs.github.com/en/graphql/reference/pulls#pullrequestreviewthread), and [Azure PR threads API](https://learn.microsoft.com/en-us/rest/api/azure/devops/git/pull-request-threads).

Findings can trigger bounded fix/review follow-ups; fixes can push to the PR branch. Fresh context is collected for each attempt. A changed PR head or target branch stops the pinned job; a base change during review prevents publication. Fix pushes use an explicit head lease to avoid overwriting concurrent changes. Start a new review when a job reports stale PR context. Fork PR writes remain unsupported. Publication retries find the existing job/commit marker before creating another review; no approval or merge action is performed.

The bundled OpenHands worker includes Rust/Cargo and Node/npm. `repository-tests` detects root Rust projects (`cargo test --locked`) or npm projects (`npm ci` then `npm test`), requiring lockfiles and a test script. Other languages/package managers need a configured validation profile and corresponding worker tools. Missing executables now include the program and operating-system error in the receipt.

Existing installations must rebuild the server and OpenHands worker, then publish and activate a new `pr-review` release containing the updated review and fix workflows and `review@3` prompt. Restarting alone does not update registry releases. Existing jobs retain their pinned image, commands and workflows; launch a new review after upgrading.

AWS examples are in [deploy/aws/README.md](deploy/aws/README.md). The current deployment is local Docker. An external deployment needs its own authentication, HTTPS, networking, credential, backup, and acceptance-test setup.

## Portal connector configuration

Open **Connectors**, then use **Workspace access** with your local maintainer token. The local maintainer now has the `connector_admin` role. Other accounts need that explicit role in `FACTORY_IDENTITIES`; observer/operator/approver alone cannot read or edit connector configuration.

1. Choose **Configure** for Azure DevOps, GitHub, Jira Cloud, or Slack.
2. Enter the service URL and credentials. For GitHub use `https://github.com` and `https://api.github.com`; for Azure DevOps use your organization URL, such as `https://dev.azure.com/example`.
3. Enable the connector. Repository access follows provider credentials; there is no repository checklist.
4. Click **Test connection** to check authentication before saving. This uses the current form, including saved secrets for blank fields, without saving or enabling the connector.
5. Save. Changes take effect without a server restart. Saving validates fields but does not run a connection test automatically.

Secret fields show whether a value is saved. Leave them blank to preserve it, enter a replacement to rotate it, or select **Clear saved credential** to remove it. Disabling a connection prevents its use, including environment fallback. Concurrent edits are rejected; reopen the form to load the latest version.

| Connector | Used for |
| --- | --- |
| Azure DevOps | Resolve repository revisions/PR context, clone, push, and open PRs for Azure Repos repositories accessible to the credentials. |
| GitHub | Repository operations and GitHub issue webhooks. Optional webhook signing secret verifies `/hooks/github`. |
| Jira Cloud | Fetch issue details at job submission using site URL, account email, and API token. Scoped personal and service-account tokens also require the site's Cloud ID. Optional webhook bearer secret verifies `/hooks/jira`. |
| Slack | Approval notifications with a bot token/default channel and `/factory` callback signature verification. A repository's configured channel overrides the default. |

For a Jira service account, enter its email and scoped API token, keep **Site URL** as `https://your-team.atlassian.net`, and enter the site's UUID in **Cloud ID** (not the organization ID). With a Cloud ID, connection tests and issue requests use `https://api.atlassian.com/ex/jira/{cloudId}`; browser issue links still use the site URL. This also supports scoped personal tokens. Leave Cloud ID blank for an unscoped token; existing configurations continue to work without changes. The token must permit reading issues and the current user (used by **Test connection**), and the account must have access to the relevant Jira projects/issues. See [Atlassian's service-account token setup](https://support.atlassian.com/user-management/docs/manage-api-tokens-for-service-accounts/). OAuth client credentials are not supported by this API-token setup.

Slack still requires creating/installing a Slack app with `chat:write` and slash-command support, inviting it to the channel, and configuring its request URL. For URL-based repositories, give the matching `slack:USER_ID` identity the `approver` role in `FACTORY_IDENTITIES`. Jira's hook still expects the normalized payload documented in [TECHNICAL.md](TECHNICAL.md), rather than an arbitrary raw Jira webhook. These forms do not install external apps or create provider tokens.

There is one connection per provider in this version. Repository onboarding is not required. Submit an HTTPS repository URL; optional YAML aliases preserve existing branch, workflow, and maintainer overrides. Azure DevOps/GitHub repository and API URLs must be inside the saved connector URL scope. LLM model configuration and its API key remain unchanged.

### Test connection

The test action is restricted to connector administrators. It reports individual results for the primary token and, when configured, a separate write token. Requests have a 10-second timeout, do not follow redirects, and return fixed error messages rather than provider response bodies or credentials. Stale configuration revisions must be reloaded before testing.

| Provider | Authentication check | What it does not verify |
| --- | --- | --- |
| Azure DevOps | Organization `/_apis/connectionData` returns an active authenticated identity. | Per-repository access, push, or PR permissions. |
| GitHub | `/user` returns an authenticated user. Supports user/PAT tokens. | GitHub App installation tokens, per-repository access, or write permissions. |
| Jira Cloud | `/rest/api/3/myself` returns an authenticated account. | Access to specific projects or issues. |
| Slack | `auth.test` accepts the token and identifies a workspace/user. | Channel membership or posting permission; no message is sent. |

Webhook/signing secrets are listed as **not tested**: they require a matching incoming callback. A successful token test does not establish that the entire connector is operational. Results describe the current form snapshot and disappear when it is edited; they are not persisted as connector health status. Testing also works for disabled connectors and does not enable them.

Provider references: [Azure DevOps connection identity](https://learn.microsoft.com/en-us/javascript/api/azure-devops-extension-api/connectiondata), [GitHub authenticated user](https://docs.github.com/en/rest/users/users#get-the-authenticated-user), [Jira current user](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-myself/), [Slack auth.test](https://docs.slack.dev/reference/methods/auth.test/).

### Storage and extensibility

Connector settings are stored in PostgreSQL with AES-256-GCM encryption. The encryption key is derived with domain separation from the existing `FACTORY_WORKER_SECRET`; **back up this secret together with the database**. Replacing it without migrating encrypted connections makes them unreadable. Connector credentials are excluded from API responses, job snapshots, and audit records. Workers receive only the repository credential authorized for their phase; Jira and Slack credentials stay on the server.

Environment variables remain a compatibility fallback only while a provider has no portal record. Existing environment credentials are not imported automatically. PostgreSQL password, server identities, and the server master secret remain bootstrap configuration.

Provider fields are declared in `src/connectors.rs::definitions`. New connectors add a schema and runtime adapter; the same portal form, encryption, access control, optimistic versioning, and audit storage are reused. The API is `GET /api/connectors`, `PUT /api/connectors/{kind}`, and `POST /api/connectors/{kind}/test` and requires `connector_admin`. The `connector_audit` table records provider, revision, actor, and time without credential values.

## 8. Troubleshooting

| Symptom | Action |
| --- | --- |
| Portal unavailable | Start Docker, check `docker compose ps`, start db/server, check `/api/health` and server logs. |
| Old model used | Rebuild/recreate server after YAML edits. Old jobs retain snapshots; submit a new job. |
| Key changes ignored | Refresh shell environment and recreate server. Do not print `.env` or full container environments. |
| Job remains queued | Inspect server outbox logs, Docker availability, worker images, and network `factory_workers`. Launch retries are bounded. |
| Worker launch timed out | No worker claimed the attempt within its 2-minute launch window. Check Docker, the job's pinned image, and worker connectivity. The phase did not start. |
| Phase execution timed out | The worker started, then exceeded that phase's time allowance. Inspect its work and configured phase timeout. |
| No worker at approval | Expected. Read the artifact and submit a decision. |
| Approval rejected | Refresh job; check exact gate digest, pending status, expiry, identity roles, and maintainer membership. |
| Agent `error` | Check model/key/access/endpoint and run isolated live check. Provider details are suppressed from receipts; generic error does not identify the precise cause. |
| Agent `invalid_output` | Model finished without the required valid file/report. Inspect task/prompt; prose success is insufficient. |
| Timeout / iteration limit | Check scope and limits. Deliberately increasing them may increase API spend. Submit a new job after changes. |
| Validation executable missing | Install repo tools in the OpenHands image, rebuild, and verify validation command. |
| Slack errors | Configure Slack or use `[api]` channels for new jobs. |
| Failure after downtime | Inspect attempts. Lost/expired workers consume automatic retries; no manual resume endpoint exists. |

Workers may already be removed by the time you investigate. Job attempts, artifacts, and receipts are the durable record.

In **Jobs & receipts → Timeline**, each attempt shows its launch or execution timeout, queued/start timestamps, and the corresponding deadline. Launch time starts when an attempt is queued; execution time starts when the worker claims it; approval time starts when the gate is created. Waiting for approval does not consume the next phase's execution allowance. Older generic timeout records are explained using their saved worker start timestamp.

## 9. Tests and files

No paid API calls are made by these checks:

```powershell
docker compose up -d db
./scripts/test.ps1
./scripts/test-openhands.ps1
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Rust checks require a host Rust toolchain; the JS syntax check requires Node.js. The OpenHands checks require the built image and use a local mock provider with the real SDK/tools. Database tests use a separate `factory_test` database. Baseline verification passed 26 Rust/database tests and seven harness tests. The live OpenAI check passed separately; full repository delivery and AWS remain unverified.

| Path | Purpose |
| --- | --- |
| `.env` | Local credentials, ignored by Git and Docker build context. |
| `config/platform.yaml` | Active profiles/model, validations, repository registry. |
| `compose.yaml`, `Dockerfile` | Services, environment wiring, images, installed tools. |
| `workflows/` | Phases, permissions, retries, gates, follow-ups. |
| `prompts/` | Versioned task instructions. |
| `harnesses/openhands/` | Adapter, pinned Python dependencies, mock tests, live check. |
| `scripts/` | Setup, environment loading, demo, restart, test helpers. |
| `web/` | Running portal. |
| `factory-floor.html` | Original standalone simulation, not the running application. |
| `TECHNICAL.md` | Architecture, API reference, connector details. |

Current boundaries: sequential workflows, 10 MiB artifacts, latest-500 observation, administrator-managed YAML, no portal job-submission controls, no automatic merge, and no total-dollar budget enforcement.

### Launch a workflow from the portal

Connect a Factory API identity with the `operator` role, then click **Run workflow**. Choose a workflow, enter a GitHub or Azure DevOps HTTPS repository URL (or an existing alias), and enter task details. Manual tasks, Jira issues, and existing PRs are supported. Workflows requiring parent artifacts are excluded. Factory resolves the default branch and pins the commit; PR inputs pin the provider-reported PR head. A successful launch opens job details. Retrying an unchanged form after an uncertain response reuses the submission key.

Configure provider credentials once in **Connectors**. GitHub URLs use `https://github.com/owner/repo`; Azure DevOps URLs use `https://dev.azure.com/org/project/_git/repo` under the connector organization URL. Enterprise GitHub uses the configured web and API endpoints. Provider permissions determine accessible repositories; saved legacy connector repository selections are ignored. Disabled connectors remain disabled.

Factory roles still control who may submit, observe, or approve jobs. Use `repositories: ["*"]` in a Factory identity to cover any repository accessible through provider credentials, or use exact URL scopes. For repositories without an alias, the `approver` role grants approval authority. Existing alias maintainer lists remain effective. Slack uses its default connector channel (or `FACTORY_SLACK_CHANNEL` for environment-based setup), with no repository selection.

The optional platform `default_workflow` (default `research-plan`) selects intake for GitHub label events and Jira hooks without a workflow. Jira hooks also accept an explicit `workflow`. The platform `repositories` map may be omitted entirely.

### Clearing Needs attention

Operators can dismiss failed, timed out, rejected, or cancelled jobs from the Factory floor. Dismissal is saved on the server for all portal sessions and recorded in the activity history. Job outcomes, artifacts, and receipts are retained. Open the job in Jobs & receipts and select Restore to Needs attention to undo a dismissal. Active jobs cannot be dismissed.


Jira issue lookup: in **Run workflow**, choose **Jira issue** and type at least two characters in the issue-key field. Factory queries Jira's issue picker after a short pause and shows up to 20 matching keys and summaries. Select a result to fill the key and title; submission fetches the current issue details. Search requires an operator identity and an enabled Jira connector, and uses the connector account's Jira visibility. The API is `GET /api/jira/issues?query=...` (2–120 characters).

Jira additional fields: open **Connectors → Jira Cloud → Additional issue fields** and click **Load fields from Jira**. Search by name or ID, add fields, edit their headings, and reorder them. **Preview task details** uses the current form credentials and mappings without saving or enabling the connector, and reports included, empty, or unavailable fields. Save the connector to apply. Up to 20 fields are stored by stable Jira ID. Description comes first; nonempty additional fields are appended under headings. Rich text and common option/user/list values are converted to readable text. Portal selection, submission, and Jira webhook intake use the same formatter; each submitted job retains its combined issue snapshot. Loading fields requires the Jira field-list API permission (classic `read:jira-work`).


### Versioned prompts and workflow releases

Open **Workflows** or **Prompts** under **Design** in the portal. Workflows appear once each; examples and fixture tests have a separate filter. Each workflow has Overview, Draft, Versions and Runs tabs. Configuration uses PostgreSQL by default (`FACTORY_CONFIGURATION_SOURCE=registry`). On the first start only, existing YAML and Markdown files are imported atomically. Later deployments and restarts do not overwrite the registry or require those files. Back up PostgreSQL to preserve drafts, releases and activation history.

Grant `configuration_editor` to people who may save drafts, validate definitions and run fixture tests; grant `configuration_publisher` to people who may publish prompt revisions and activate/restore workflow releases. These are global configuration permissions, separate from repository operator and approval permissions. New local setup credentials include both roles; existing installations must add them to the intended identity in `FACTORY_IDENTITIES` and restart once.

1. Open a workflow and choose **Edit draft**, or use **New workflow** / **Import** from the list.
2. Edit instructions in **Edit**. The phase outline shows where they are used; **Advanced YAML** exposes workflow structure and dependencies.
3. Continue to **Review** to inspect highlighted changes. Changed prompt and workflow versions are assigned automatically, references are updated, and the candidate is saved and validated.
4. In **Check & test**, optionally run a **fixture test**. This snapshots the candidate and runs fixture agents only in `local-demo`, with operator permission required. Gates still require normal approval. Test status and a link to the run appear against the candidate; later edits make earlier results visibly out of date.
5. In **Publish**, add a change note and choose **Publish and activate** for new jobs, or **Publish only** to retain the current active release. Publication and optional activation complete atomically with closing the saved draft.
6. Use **Versions** to compare, export, activate an unpublished-to-runtime release, or **Restore this release** with a reason. Release numbers are readable and scoped to each workflow.

Drafts support explicit saves, unsaved-change navigation protection, and discarding work in progress without deleting release history.

A release contains the root workflow, all reachable follow-up workflows (including bounded cycles), and their exact prompts. Revisions cannot be overwritten with different content; repeated identical imports reuse the release. Draft saves and activation use optimistic concurrency. A stale import is rejected if the active generation advanced: export the current release and reapply/review your changes before publishing. Failed validation or publication leaves the active release unchanged.

Rollback changes only the release selected for future jobs. Queued/running jobs, retries, approval waits, and follow-up jobs keep their original snapshot. Replaying a submission's idempotency key returns its original job across release changes. A fresh run uses a fresh key. Receipts identify the release ID and digest alongside the exact runtime snapshot. Restoring a release does not reverse effects from executed jobs.

The **Prompts** library groups revisions by logical name and shows consuming workflows. Edit instructions, review the diff, and select which consumers should receive review drafts. Publication automatically assigns the next prompt revision and creates only those selected drafts; active workflows stay unchanged until their drafts are published and activated. Consumers with an existing draft must adopt the revision inside that draft. New workflows combining existing capabilities require no deployment; new capabilities, integrations and platform profiles still require code/configuration deployment. Releases resolve allowed platform profiles at submission; the resulting job pins their exact values and worker image digest. They do not freeze runtime credentials.

### Offline workflow development

Export a release from the portal as a JSON bundle. The CLI unpacks it to editable YAML and Markdown plus `release.lock`, without a portal connection or database:

```powershell
cargo run --bin factory-bundle -- unpack config/platform.yaml research-plan-bundle.json ./local-workflow
cargo run --bin factory-bundle -- pack config/platform.yaml research-plan ./local-workflow ./candidate.json --locked
cargo run --bin factory-bundle -- validate config/platform.yaml ./candidate.json
```

`--locked` rejects content changes against the exported digest. To package intentional edits, omit `--locked` and import `candidate.json` into the portal as a draft; portal review assigns changed revisions automatically. Unpack requires a new directory to avoid overwriting local work. YAML formatting/comments do not affect release identity; prompt bytes do. The bundle contains no credentials or platform overrides. Local validation requires compatible profiles in the supplied platform configuration.

For a new local workflow, place YAML under `workflows/` and Markdown under `prompts/`, then use `pack` with their parent directory and the root workflow ID. Every reference must exist locally; no implicit network fetch or production fallback occurs.

To execute local definitions through a development control plane, set `FACTORY_CONFIGURATION_SOURCE=files`, `FACTORY_WORKFLOWS` and `FACTORY_PROMPTS` before starting `factory-server` with a development platform (`allow_fixture: true`). Files are loaded at startup; restart after editing. This mode does not seed or modify the portal registry and disables portal configuration mutations. Execution still needs the normal PostgreSQL, worker and provider setup; offline bundle validation does not. Use a separate development database when testing locally. Portal fixture tests do not validate live-model quality or provider connectivity.
