-- Membership: a user's membership in an organization (CLAUDE.md Entities:
-- Membership). `instrument_ids` / `principal_instrument_ids` are `uuid[]`
-- referencing Instrument by id -- Postgres cannot enforce element-level FKs
-- on an array, so these are validated against the live Instrument set in
-- the service layer on write (see CLAUDE.md Decisions: Authorization /
-- Membership FK-integrity note). `is_principal` is orthogonal to `role`.
CREATE TABLE membership (
    id                          uuid PRIMARY KEY,
    user_id                     uuid NOT NULL REFERENCES "user" (id),
    organization_id             uuid NOT NULL REFERENCES organization (id),
    role                        text NOT NULL
        CHECK (role IN ('owner', 'archivist', 'conductor', 'musician')),
    instrument_ids              uuid[] NOT NULL DEFAULT '{}',
    is_principal                boolean NOT NULL DEFAULT false,
    principal_instrument_ids    uuid[] NOT NULL DEFAULT '{}',
    created_at                  timestamptz NOT NULL DEFAULT now(),
    updated_at                  timestamptz NOT NULL DEFAULT now(),
    created_by                  uuid
);

CREATE INDEX membership_organization_id_idx ON membership (organization_id);

-- Unique constraints table (CLAUDE.md): Membership (user_id,
-- organization_id). No soft-delete on this entity.
CREATE UNIQUE INDEX membership_user_id_organization_id_key ON membership (user_id, organization_id);
