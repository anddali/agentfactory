# Factories — technical reference

For everyday operation and the verified current installation state, start with [README.md](README.md).

A Rust control plane, disposable phase workers, PostgreSQL, content-addressed artifact storage, and an observation portal expanded from `factory-floor.html`.

The local `demo` workflow uses a **fixture agent**. Scheduling, worker containers, database transactions, human gates, artifacts, and receipts are real; fixture text is clearly labelled. The separate `coding-default` profile uses OpenHands with `openai/gpt-5.6-luna`, and its live file-writing check passed. Only the fixture repository is registered; real repository delivery, Slack and AWS still require configuration and acceptance checks.

## Run locally

Requires Docker Desktop/Engine with Compose. PowerShell 7 is used by the convenience scripts.

```powershell
./scripts/setup.ps1
docker compose build server worker-image openhands-image
docker compose up -d
./scripts/smoke.ps1
```

Open **http://127.0.0.1:8787**. Port 8787 avoids a reserved port range on this Windows machine; override it with `FACTORY_PORT` in `.env`. Worker-to-server traffic still uses port 8080 inside the Docker network.

The smoke test submits the same request twice, runs three fresh workers, approves two exact artifact versions through the authenticated API, replays both approvals, downloads a receipt, and verifies the final artifact's SHA-256. It only submits the `demo` workflow against `local-demo`.

```powershell
./scripts/smoke.ps1 -LeavePending   # leave fixture research at its human gate
./scripts/restart-check.ps1        # verify a pending gate survives server restart
docker compose logs --tail 100 server
docker compose stop               # retain database and artifact volumes
```

`setup.ps1` generates local credentials in ignored `.env`; rerunning it preserves them. The portal supports authenticated maintainer gate decisions; local Compose allows unauthenticated observation on loopback. Job submission, decisions, cancellation, and worker endpoints always require credentials. The original self-contained simulation remains in `factory-floor.html` for comparison; `web/` is the live portal.

## What is implemented

| Capability | Implementation |
| --- | --- |
| Two binaries | `factory-server` serves the API/UI, coordinator, and callbacks. `factory-worker` claims and executes one phase, then exits. |
| Durable state | PostgreSQL case/job documents, ordered audit events, request deduplication, and transactional dispatch outbox. No in-memory execution state is required for recovery. |
| YAML workflows | Strict `factory/v1` schema, sequential phases, ordered capabilities, typed top-level inputs, declared artifact references, permission checks, bounded retries, gates and follow-ups. |
| Execution provenance | Jobs pin definitions, prompt content/hashes, worker image digest, agent/worker profile versions, policy version and platform build revision. |
| Phase attempts | Fresh IDs and workers for retries. An instance must claim its attempt. Superseded, expired, cancelled, and duplicate-but-different results are rejected. |
| Human gates | Persistent records bound to the exact phase attempt and artifact-set hash. Authenticated maintainers decide through the API or signed Slack slash command. No worker waits for approval. |
| Reconciliation | Persisted launch, phase, heartbeat, and approval deadlines. Lost workers and launch failures consume bounded retry budgets. Cancellation revokes authority and schedules cleanup. |
| Local execution | Docker containers use approved image digests, resource limits, a non-root user, dropped capabilities, a read-only root filesystem, and temporary workspaces. |
| AWS execution/storage | ECS RunTask uses attempt IDs as idempotency tokens; immutable task definition/image checks; ECS StopTask; S3 get/put adapter. See `deploy/aws/`. |
| Artifacts | SHA-256 storage, immutable names within each attempt, declared-input downloads, path containment, size limits and integrity verification. |
| Repository capabilities | Git checkout at a pinned commit, deterministic branch identity, fast-forward-only push, GitHub/ADO PR lookup before creation. Same-repository PR inputs resolve the source SHA and branch. |
| Agent backends | OpenHands SDK 1.49.2 with model selection, limits, and execution receipts; versioned `command` profiles for alternative harnesses; fixture profile for local acceptance tests. |
| Connectors | Signed GitHub issue-label webhooks; authenticated Jira Automation payloads; signed Slack decisions with timestamp and maintainer/channel checks. |
| Portal | Overview, case floor, production definitions, jobs, timelines, gates, receipts, artifact downloads, activity, platform details, repository/workflow filters, search, and light/dark themes. Authenticated maintainer approval/rejection controls. |

## Execution and recovery

```text
external request -> case -> pinned job -> transactional outbox -> phase worker
                                                 ^                 |
                                                 |      artifacts + result
                                                 |                 v
                                           next phase <- durable gate
```

`Store::mutate` locks the job row, applies a pure state transition, then commits the updated document, events, any follow-up job, and dispatch effects in one transaction. A failed transaction cannot publish a start effect without its job state. Multiple server instances use row locks and `SKIP LOCKED` for dispatch/reconciliation; advisory locks serialize matching request keys and per-attempt start/stop effects.

Docker container names and ECS client tokens are derived from attempt IDs. If dispatch succeeds and the server crashes before recording success, dispatch retries find the same execution. Workers have per-attempt HMAC credentials and an instance fence; a second instance cannot claim an already owned attempt. A heartbeat every ten seconds maintains liveness; a missing heartbeat expires after ninety seconds. Launches have a two-minute deadline. Phase and approval timeouts come from the pinned workflow.

Gate expiry and cancellation reject later decisions/results immediately, even before the scheduler's next pass. A duplicate completion with the same content is accepted without advancing twice. A duplicate decision must preserve its actor, channel, gate, artifact digest and outcome. A retry gets a new attempt ID and fresh workspace. Review/fix follow-ups keep the case ID, pass their artifacts through durable storage, and use bounded depth; unresolved findings at the bound require attention.

Repository policy is `pin_at_job_start`: approval does not silently switch to the latest branch. Follow-up jobs can use the exact commit reported by the preceding authorized push. This initial version does not implement “invalidate approvals when upstream changes”; select a new job explicitly when that is needed.

## Configure a real repository and agent

1. Build the OpenHands worker with `docker compose build openhands-image`. Extend its `openhands-worker` Docker stage with your repository's language toolchain and validation tools (for example Node.js). The supplied image contains Python, Git, the Rust worker, and the pinned OpenHands SDK and tools.
2. Configure `agents.coding-default.openhands` as described below. The bundled adapter receives the resolved prompt, issue snapshot, approved input content, and output filename. It runs in the checked-out repository and must write the requested output file. The worker handles Git publication and PR creation. `review` must also write `review.json` with a `findings` array. The existing `backend: command` interface remains available for other harnesses.
3. Set `env_keys` to the agent's required credential variable names, and supply those secrets to the server. Agent children get only those variables plus basic runtime environment. They do not inherit the worker callback credential or repository token.
4. Register the repository in `config/platform.yaml`, or use `config/production.example.yaml` as a starting point. GitHub `api_url` is `https://api.github.com/repos/OWNER/REPO`; ADO uses `https://dev.azure.com/ORG/PROJECT/_apis/git/repositories/REPOSITORY_ID`. Supply separate provider credentials through `read_token_env` and `write_token_env`.
5. Set the validation profile to an executable/argument array available inside the image. Commands are administrator configuration, never arbitrary strings taken from an issue or webhook.
6. Configure repository maintainers, Slack channel, and API identities. Add repositories to each identity's authority list. Set `allow_fixture: false` for production and use a workflow directory that excludes `demo.yaml`.
7. Rebuild/restart the server to load changed definitions. Existing jobs retain their pinned definitions and image digests. Retain those images and artifact objects until their jobs finish.

Secrets are delivered to a successfully claimed worker in its authenticated manifest according to phase permissions. They are not persisted with jobs, receipts, or dispatches, nor passed as Docker/ECS environment overrides. Use HTTPS outside the isolated local Docker network. Scope the provider credentials themselves: a manifest cannot reduce the capabilities of an overprivileged provider token.

Each new capability belongs in Rust (`workflow::capability` plus the worker implementation). Combinations of existing capabilities need only YAML and prompts. Unknown fields, tasks, prompts, profiles, permissions, forward artifact references, invalid paths, missing parent outputs, and unbounded retries/follow-ups are rejected at startup.

### OpenHands harness

`coding-default` now uses OpenHands SDK **1.49.2**. Direct dependencies are listed in `harnesses/openhands/requirements.txt`; the complete tested Python dependency set is pinned in `requirements.lock`. Docker/ECS image digests pin the installed runtime for each job. The `demo` workflow continues using its separate fixture worker and does not call a model.

Set `LLM_API_KEY` in the local ignored `.env` file. Compose supplies it to the server, which returns it only in a claimed worker manifest. Do not put keys in YAML, prompts, endpoint URLs, or source control. Set `model` to a provider/model identifier supported by your account; the shipped model is an example, not a requirement. `base_url` optionally selects your gateway or local endpoint. `api_mode` accepts `auto` (default), `chat`, or `responses`; use `chat` for gateways that only implement Chat Completions. The current adapter uses API-key authentication; cloud workload-identity authentication requires an additional adapter configuration.

```yaml
agents:
  coding-default:
    version: 1
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

To use different models by phase, define additional agent profiles and set `agentProfile: profile-name` on a workflow phase. Omission inherits `defaults.agentProfile`. All profiles and phase selections are pinned in the job snapshot. Changing a profile affects newly submitted jobs. Use an OpenHands-capable worker image for every phase that selects this backend.

`max_output_tokens` limits each model response; it is not a total token or dollar budget. Iterations and wall-clock execution are bounded separately. Provider retries are disabled in the adapter; Factories controls phase retries. There is no automatic model fallback. Receipts record the selected provider/model identifier, actual installed harness version, completion status, and SDK-reported token usage. A provider alias may route internally; the receipt cannot attest to undisclosed provider routing. Successful OpenHands completion requires matching provenance and the output artifact; iteration exhaustion, missing output, provider errors, and timeouts fail the phase.

The harness runs inside the existing nonroot container with a read-only root filesystem and disposable writable workspace. Tool selection controls the SDK tools exposed to the model; a shell tool still has the container's filesystem and network authority. Add deployment network policy if your repository requires restricted egress. Provider diagnostics are suppressed from receipts to avoid copying credentials or repository content into logs.

After configuring the repository, maintainer identity, model and credentials:

```powershell
docker compose build server openhands-image worker-image
docker compose up -d server
./scripts/test-openhands.ps1
```

The OpenHands tests use the real installed SDK, a local mock model HTTP endpoint, and real tool execution under the container restrictions. They require no paid credentials and cover successful output, missing output, provider errors, iteration limits, deadlines, and version mismatch. A paid-provider acceptance run is still required for each model you enable. The portal receipt view displays recorded agent runs and token usage.

### Local Ollama

Docker workers reach Ollama on the Windows host through `http://host.docker.internal:11434/v1`. Set the OpenHands profile's `model` to `openai/NAME_FROM_OLLAMA_LIST`, `base_url` to that address, and `api_mode: chat`. Set `LLM_API_KEY=ollama` in `.env` (local Ollama ignores this placeholder). The `openai/` prefix selects the protocol; it does not send requests to OpenAI.

`reasoning_effort` is optional and defaults to null: no reasoning option is sent. Set it explicitly only for models that support it. Tool-calling support alone does not guarantee reliable autonomous coding; test the selected model on representative tasks before using it on a real repository.

## API

Observation uses an identity with the `observer` role unless `FACTORY_PUBLIC_READ=true`. `FACTORY_IDENTITIES` is a JSON array of `{token, subject, roles, repositories}`; roles are `observer`, `operator`, and `approver`. For optional aliases, an approver must also appear in the alias's `maintainers` list. URL-based repositories use identity approver scopes. `*` explicitly grants access to all repositories.

| Method / path | Purpose |
| --- | --- |
| `GET /api/health` | Database readiness and executor type |
| `GET /api/catalog` | Definitions and visible repositories |
| `GET /api/jobs` | Latest 500 visible job summaries |
| `POST /api/jobs` | Submit a job; operator token and `Idempotency-Key` required |
| `GET /api/jobs/{id}` | Full job, attempts, artifacts, gates and events |
| `GET /api/jobs/{id}/receipt` | Downloadable execution provenance |
| `POST /api/jobs/{id}/decisions` | Maintainer decision against a gate/digest |
| `POST /api/jobs/{id}/cancel` | Cancel active work; operator token required |
| `GET /api/events` | Latest 500 visible events |
| `GET /api/artifacts/{id}` | Authorized artifact download |
| `POST /worker/{attempt}/claim` | Claim an attempt and obtain its scoped manifest |
| `POST /worker/{attempt}/heartbeat` | Renew the owning worker's liveness |
| `PUT /worker/{attempt}/artifacts/{name}` | Publish a declared artifact |
| `GET /worker/{attempt}/inputs/{name}` | Download a declared phase input |
| `POST /worker/{attempt}/complete` | Fenced, idempotent completion report |

Job request:

```json
{
  "workflow": "research-plan",
  "repository": "example-service",
  "issue": {
    "provider": "jira",
    "key": "ENG-123",
    "title": "Handle duplicate registrations",
    "body": "Acceptance criteria and normalized issue context"
  }
}
```

For an existing PR, use `workflow: "pr-review"`, `issue.provider: "github_pr"` or `"ado_pr"`, and its numeric ID as `issue.key`. The coordinator retrieves the authoritative PR head from the credentialed provider. Fork PR writes are deliberately unsupported without a separately registered authority boundary.

Decision request (`Authorization: Bearer <maintainer-token>`):

```json
{
  "event_id": "unique-external-decision-id",
  "gate_id": "gate-uuid-from-the-job",
  "artifact_digest": "exact-digest-from-the-gate",
  "approve": true
}
```

The local demo permits API decisions. Production research/plan definitions permit Slack and API. Rejection stops the job.

## Connector setup

- **GitHub:** send `issues` label events to `/hooks/github`, configure `FACTORY_GITHUB_WEBHOOK_SECRET`, and label an issue `agent-ready`. The webhook verifies `X-Hub-Signature-256` against the raw body and uses `X-GitHub-Delivery` for deduplication. Repository clone URLs resolve through the GitHub connector; optional aliases can override the intake workflow, otherwise `default_workflow` applies. Submit PR reviews through the normalized API using the PR input described above.
- **Jira Automation:** POST to `/hooks/jira` with `Authorization: Bearer <FACTORY_JIRA_WEBHOOK_SECRET>`. Send `{event_id, repository, issue:{key, fields:{summary, description, labels}}}`. `repository` is an HTTPS repository URL or optional alias, and `event_id` must identify the source event consistently across retries. Requests without the `agent-ready` label are ignored. Description accepts plain text or an Atlassian document object.
- **Slack:** configure a bot token, signing secret, repository channel, and maintainer subjects such as `slack:U123`. Set `FACTORY_PORTAL_URL` to the public HTTPS origin of the portal to include a report link in approval notices. Create a `/factory` slash command targeting `/hooks/slack`. The outbox posts an approval request; maintainers use `/factory approve <gate-id> <artifact-digest>` or `reject`. The handler verifies the five-minute signature window, user identity, channel, gate and artifact version. Slack delivery is at least once; the stable gate ID makes repeated notices recognizable and decisions remain idempotent.

There is no Jira polling service in this version: intake is event driven. Standard Jira/GitHub events are normalized before workers execute `issue.fetch`; that task writes the pinned issue snapshot rather than fetching changing issue content midway through a job.

References: [GitHub PR REST API](https://docs.github.com/en/rest/pulls/pulls), [ADO PR API](https://learn.microsoft.com/en-us/rest/api/azure/devops/git/pull-requests/get-pull-request?view=azure-devops-rest-7.1), [Slack request verification](https://docs.slack.dev/authentication/verifying-requests-from-slack/).

## Develop and test

Rust stable and PostgreSQL 17 are used locally. `Cargo.lock` is included for repeatable dependency resolution. CI runs formatting, Clippy, unit tests, PostgreSQL tests and JavaScript syntax checks.

```powershell
docker compose up -d db
./scripts/test.ps1
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Database tests use a separate `factory_test` database, unique request IDs, and do not truncate application data. To run them without the helper, set `DATABASE_URL` to a disposable database and run `cargo test --test database -- --ignored`.

For native server development, dot-source `scripts/env.ps1`, set `DATABASE_URL` to the Compose PostgreSQL port 54329, and run `cargo run --bin factory-server`. Set `FACTORY_WORKER_SERVER_URL=http://host.docker.internal:PORT` and bind the server to `0.0.0.0:PORT` for Docker workers. An explicit `FACTORY_EXECUTOR=process` option exists only for trusted fixture workflows when container execution is unavailable; it is not an isolation boundary for repository code.

Current limits are sequential workflows, 10 MiB per artifact, a latest-500 observation window, trusted administrator workflow/profile configuration, and no automatic merge operation. AWS and external provider adapters are included but need an environment-specific acceptance run with your credentials. Local tests do not establish that your repository's build/test workload fits Fargate.

## Source map

`src/workflow.rs` validates definitions; `src/engine.rs` contains pure transitions; `src/store.rs` commits state and effects; `src/execution.rs` dispatches and reconciles external execution; `src/worker.rs` implements task capabilities; `src/api.rs` handles observation, authority and callbacks; `src/storage.rs` implements filesystem/S3 storage. `workflows/`, `prompts/` and `config/` are versioned inputs. `web/` is the dependency-free portal.
