# Factory in Slack

## User journey

1. Type `/factory` and choose a workflow.
2. For a PR review, paste the GitHub/Azure PR URL and optionally a related ticket. For ticket workflows, enter a Jira key or supported issue link and select an authorized repository. Type into the repository picker to search aliases or paste an authorized HTTPS URL (up to Slack's 150-character option limit; use an alias for longer URLs).
3. Factory loads the actual title, resolves access, and shows the repository, configured update channel, and possible writes, including follow-up fixes. **Edit details** preserves the form. **Start review / Start workflow** starts the job; previewing alone does not.
4. One channel message tracks the run, including its review/fix follow-ups. Pending approvals and terminal outcomes also receive thread updates. Normal progress updates edit the message without posting more channel messages.
5. **Review approval** opens the report from the exact phase attempt. Read it, choose **Approve and continue** or **Reject and stop**, and confirm. Rejection stops the job; it does not request revisions. Already-decided and expired gates cannot be decided again.

Reports are shown as plain text to prevent untrusted content creating Slack mentions or actions. Large reports are explicitly labelled as excerpts and link to the full authenticated portal report. If a full report would be omitted and no HTTPS portal URL is configured, Factory does not expose an approval form for the excerpt alone. Portal availability and access must be verified during setup. The original artifact digest stays server-side throughout.

## Install or update the Slack app

1. Expose the Factory server through your existing HTTPS ingress. Slack cannot reach the default localhost-only Docker port. Do not expose PostgreSQL or the Docker socket.
2. Replace `factory.example.com` in [manifest.yaml](manifest.yaml) with the public Factory origin. Import it into Slack's app configuration, or apply its settings to the existing app. Install/reinstall into the workspace after changing scopes. The app needs `commands` and `chat:write` only; it does not read channel history.
3. Set `/factory`'s Request URL to `https://YOUR_ORIGIN/hooks/slack`. Enable Interactivity with both the Request URL and Options Load URL pointing at `https://YOUR_ORIGIN/hooks/slack/interactions`. The options URL powers the repository picker.
4. Invite the bot to each configured update channel, including private channels.
5. In the portal's Slack connector, save the bot token, signing secret, **Workspace ID** (`T…`), and default update channel ID (`C…`). Repository `slack_channel` overrides still apply. Workspace ID can alternatively be supplied through `FACTORY_SLACK_TEAM_ID`. Keep credentials out of the manifest.
6. Set `FACTORY_PORTAL_URL` to the public HTTPS portal origin. This enables exact job and report links. Portal access still uses the existing Factory identity token; Slack is not portal SSO.
7. Add Slack identities to `FACTORY_IDENTITIES` and restart/recreate the server to load them. Use `subject: slack:U123` with repository-scoped `operator` for starting work, `approver` for decisions, and `observer` if that identity also needs portal reads. The existing identity schema requires a random API token of at least 24 characters; never put that token in Slack. A repository alias's `maintainers` must also include the Slack subject for approval. URL-based repositories use role/repository scope checks.
8. Ensure applicable workflow gates include `slack` in their channels. For registry installations, publish and activate the updated workflow release; existing jobs retain their pinned gate definitions.
9. Rebuild and recreate the server. Database migration `0005_slack.sql` runs automatically on startup. No worker image change is needed for the Slack UI.

Legacy `/factory approve <gate-id> <artifact-digest>` and `reject` remain available. They now require the same approver role and alias maintainer checks as the portal, plus the workspace ID and configured channel. Existing installations relying only on a maintainer list must add the matching Factory identity.

## Delivery and recovery

Form sessions are bound to workspace, user, and modal. Pending previews, submissions, and modal updates are stored in PostgreSQL with a 24-hour session lifetime. Confirmation uses one idempotency key per session; repeated confirmation cannot start another job. An active workflow configuration or destination change requires another preview.

Status message IDs are persisted per root job; the latest follow-up supplies the status. Message rendering is retried independently from workflow execution, with bounded exponential backoff and HTTP `Retry-After` handling. Status updates are coalesced, so very short intermediate states can be skipped. Delivery is at least once: a crash after Slack accepts a new message but before its ID is saved can produce a duplicate notice despite stable client message IDs. Decisions and job creation remain idempotent.

The handler has a 2.8-second response budget. Provider lookups and worker/image preparation run in background tasks; a database outage returns a retryable failure instead of pretending a request was accepted. Modal delivery retries are bounded because the user may have closed the modal. Closing a confirmed start dialog does not cancel an accepted job. Use the portal to inspect or cancel it.

If Slack is unavailable, jobs and API/portal approvals continue normally. Failed status updates remain in `slack_messages` with `tries` and `available_at`. Pending UI work is in `slack_sessions`. These tables contain issue/report context, so apply the same database access and retention controls as job records. They contain no bot tokens or signing secrets.

## Verification

Run `cargo test --all-targets` and `cargo clippy --all-targets -- -D warnings`.

For the end-to-end mock test, set `DATABASE_URL` to a **disposable PostgreSQL database**, then run:

```powershell
cargo test --lib slack::integration_tests -- --ignored --nocapture
```

This uses a local fake Slack API, fixture work, and a temporary artifact directory. It exercises signed callbacks, workspace/access rejection, forms, duplicate starts, approvals, concurrent/stale decisions, delivery failure/retry, and follow-up status. It does not send real Slack messages or invoke models.

Before production use, exercise the same journey in a test Slack channel on desktop and mobile. Check repository suggestions, form edits, private-channel membership, long reports, expired approvals, a second reviewer, portal authentication, and a server restart. Mock API tests do not establish Slack's actual rendering or installation permissions.

Slack references: [app manifest](https://docs.slack.dev/reference/app-manifest/), [interactions and response deadlines](https://docs.slack.dev/interactivity/handling-user-interaction/), [modals](https://docs.slack.dev/surfaces/modals/).
