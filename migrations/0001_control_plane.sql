CREATE TABLE IF NOT EXISTS cases (
    id uuid PRIMARY KEY,
    external_key text NOT NULL UNIQUE,
    document jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS jobs (
    id uuid PRIMARY KEY,
    case_id uuid NOT NULL REFERENCES cases(id),
    status text NOT NULL,
    document jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX jobs_active ON jobs(status, updated_at);
CREATE INDEX jobs_case ON jobs(case_id, created_at);
CREATE INDEX jobs_attempts ON jobs USING gin ((document->'attempts') jsonb_path_ops);
CREATE TABLE IF NOT EXISTS requests (
    request_key text PRIMARY KEY,
    body_hash text NOT NULL,
    job_id uuid NOT NULL REFERENCES jobs(id)
);
CREATE TABLE IF NOT EXISTS events (
    sequence bigserial PRIMARY KEY,
    id uuid NOT NULL UNIQUE,
    job_id uuid NOT NULL REFERENCES jobs(id),
    document jsonb NOT NULL
);
CREATE INDEX events_job ON events(job_id, sequence);
CREATE TABLE IF NOT EXISTS outbox (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL REFERENCES jobs(id),
    document jsonb NOT NULL,
    status text NOT NULL DEFAULT 'pending',
    tries integer NOT NULL DEFAULT 0,
    available_at timestamptz NOT NULL DEFAULT now(),
    last_error text
);
CREATE INDEX outbox_pending ON outbox(status, available_at);

