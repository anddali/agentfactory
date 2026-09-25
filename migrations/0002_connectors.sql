CREATE TABLE connectors (
    kind TEXT PRIMARY KEY,
    ciphertext BYTEA NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1,
    updated_by TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE connector_audit (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL,
    revision BIGINT NOT NULL,
    actor TEXT NOT NULL,
    at TIMESTAMPTZ NOT NULL DEFAULT now()
);
