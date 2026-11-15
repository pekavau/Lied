-- Arrangement: a specific arrangement for specific instrumentation, owned
-- by an organization (CLAUDE.md Entities: Arrangement). Also the
-- edition/variant unit: multiple editions of one Work are multiple
-- Arrangement rows. `slug` is immutable per arrangement; `status` lets an
-- org retire an edition without deleting it.
CREATE TABLE arrangement (
    id                  uuid PRIMARY KEY,
    organization_id     uuid NOT NULL REFERENCES organization (id),
    title               text NOT NULL,
    slug                text NOT NULL,
    work_id             uuid REFERENCES work (id),
    instrumentation     text,
    arranger            text,
    publisher           text,
    purchase_date       date,
    license_notes       text,
    copy_count_allowed  integer,
    status              text NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'archived')),
    duration_seconds    integer,
    difficulty          smallint
        CHECK (difficulty BETWEEN 1 AND 8),
    difficulty_ratings  jsonb,
    difficulty_notes    text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    created_by          uuid REFERENCES "user" (id),
    deleted_at          timestamptz
);

CREATE INDEX arrangement_organization_id_idx ON arrangement (organization_id);
CREATE INDEX arrangement_work_id_idx ON arrangement (work_id);

-- Unique constraints table (CLAUDE.md): Arrangement (organization_id,
-- slug), live rows only.
CREATE UNIQUE INDEX arrangement_organization_id_slug_key
    ON arrangement (organization_id, slug)
    WHERE deleted_at IS NULL;
