CREATE TABLE configuration_revisions (
    kind TEXT NOT NULL CHECK (kind IN ('prompt','workflow')),
    name TEXT NOT NULL,
    digest TEXT NOT NULL,
    content TEXT NOT NULL,
    actor TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, name, digest),
    UNIQUE (kind, name)
);
CREATE TABLE workflow_releases (
    id UUID PRIMARY KEY,
    workflow TEXT NOT NULL,
    digest TEXT NOT NULL,
    bundle JSONB NOT NULL,
    actor TEXT NOT NULL,
    note TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workflow, digest)
);
CREATE TABLE active_releases (
    workflow TEXT PRIMARY KEY,
    release_id UUID NOT NULL REFERENCES workflow_releases(id),
    generation BIGINT NOT NULL
);
CREATE TABLE release_audit (
    id BIGSERIAL PRIMARY KEY,
    workflow TEXT NOT NULL,
    release_id UUID NOT NULL REFERENCES workflow_releases(id),
    generation BIGINT NOT NULL,
    actor TEXT NOT NULL,
    note TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE configuration_drafts (
    id UUID PRIMARY KEY,
    revision BIGINT NOT NULL,
    bundle JSONB NOT NULL,
    actor TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
