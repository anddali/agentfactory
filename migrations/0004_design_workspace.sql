ALTER TABLE workflow_releases ADD COLUMN release_number BIGINT;
WITH numbered AS (
    SELECT id, row_number() OVER (PARTITION BY workflow ORDER BY created_at, id) AS n FROM workflow_releases
) UPDATE workflow_releases SET release_number=numbered.n FROM numbered WHERE workflow_releases.id=numbered.id;
ALTER TABLE workflow_releases ALTER COLUMN release_number SET NOT NULL;
CREATE UNIQUE INDEX release_numbers ON workflow_releases(workflow, release_number);
ALTER TABLE configuration_drafts ADD COLUMN status TEXT NOT NULL DEFAULT 'editing' CHECK(status IN ('editing','published','discarded'));
ALTER TABLE configuration_drafts ADD COLUMN published_release UUID REFERENCES workflow_releases(id);
CREATE TABLE configuration_tests (
    job_id UUID PRIMARY KEY REFERENCES jobs(id),
    draft_id UUID NOT NULL REFERENCES configuration_drafts(id),
    candidate_digest TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
