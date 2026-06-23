-- Tag: an open-ended classifier scoped to an organization (CLAUDE.md
-- Entities: Tag). `kind` stays free-text by design (no CHECK) so orgs can
-- invent categories without a migration.
CREATE TABLE tag (
    id              uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organization (id),
    name            text NOT NULL,
    kind            text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    created_by      uuid REFERENCES "user" (id),
    deleted_at      timestamptz
);

CREATE INDEX tag_organization_id_idx ON tag (organization_id);

-- Unique constraints table (CLAUDE.md): Tag (organization_id, name, kind),
-- live rows only.
CREATE UNIQUE INDEX tag_organization_id_name_kind_key
    ON tag (organization_id, name, kind)
    WHERE deleted_at IS NULL;
