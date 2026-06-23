-- Collection: a named, indexed set of arrangements belonging to an
-- organization (CLAUDE.md Entities: Collection).
CREATE TABLE collection (
    id              uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organization (id),
    name            text NOT NULL,
    slug            text NOT NULL,
    type            text NOT NULL
        CHECK (type IN ('program', 'standing')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    created_by      uuid REFERENCES "user" (id),
    deleted_at      timestamptz
);

CREATE INDEX collection_organization_id_idx ON collection (organization_id);

-- Unique constraints table (CLAUDE.md): Collection (organization_id,
-- slug), live rows only.
CREATE UNIQUE INDEX collection_organization_id_slug_key
    ON collection (organization_id, slug)
    WHERE deleted_at IS NULL;
