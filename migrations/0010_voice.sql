-- Voice: an individual instrument part within an arrangement (CLAUDE.md
-- Entities: Voice).
CREATE TABLE voice (
    id              uuid PRIMARY KEY,
    arrangement_id  uuid NOT NULL REFERENCES arrangement (id),
    name            text NOT NULL,
    slug            text NOT NULL,
    instrument_id   uuid NOT NULL REFERENCES instrument (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    created_by      uuid REFERENCES "user" (id),
    deleted_at      timestamptz
);

CREATE INDEX voice_arrangement_id_idx ON voice (arrangement_id);
CREATE INDEX voice_instrument_id_idx ON voice (instrument_id);

-- Unique constraints table (CLAUDE.md): Voice (arrangement_id, slug), live
-- rows only.
CREATE UNIQUE INDEX voice_arrangement_id_slug_key
    ON voice (arrangement_id, slug)
    WHERE deleted_at IS NULL;
