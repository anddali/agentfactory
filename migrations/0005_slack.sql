-- Slack holds opaque session IDs only. Authority, inputs, and artifact identity
-- live on the server. Pending work survives restarts and is claimed under a lock.
CREATE TABLE slack_sessions (
    id uuid PRIMARY KEY,
    team_id text NOT NULL,
    user_id text NOT NULL,
    channel_id text NOT NULL,
    view_id text,
    stage text NOT NULL,
    data jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL DEFAULT now() + interval '24 hours',
    available_at timestamptz NOT NULL DEFAULT now(),
    tries integer NOT NULL DEFAULT 0
);
CREATE INDEX slack_sessions_pending ON slack_sessions(stage, available_at);
CREATE TABLE slack_messages (
    root_id uuid PRIMARY KEY REFERENCES jobs(id),
    channel_id text NOT NULL,
    message_ts text,
    rendered_hash text,
    milestone_hash text,
    available_at timestamptz NOT NULL DEFAULT now(),
    tries integer NOT NULL DEFAULT 0,
    finished boolean NOT NULL DEFAULT false
);
CREATE INDEX jobs_slack_root ON jobs ((document->>'root_id'), ((document->>'depth')::int));
