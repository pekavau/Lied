-- File: an actual file representing a voice or full score, in a specific
-- format (CLAUDE.md Entities: File). `voice_id` is nullable for full-score
-- files. The MinIO object key is derived from the entity tree, not stored
-- (see CLAUDE.md Decisions: Files & storage).
CREATE TABLE file (
    id                      uuid PRIMARY KEY,
    arrangement_id          uuid NOT NULL REFERENCES arrangement (id),
    voice_id                uuid REFERENCES voice (id),
    name                    text NOT NULL,
    format                  text NOT NULL
        CHECK (format IN ('lilypond', 'musicxml', 'pdf', 'image')),
    mime_type               text NOT NULL,
    derived_from_file_id    uuid REFERENCES file (id),
    conversion_quality      text
        CHECK (conversion_quality IN ('clean', 'omr', 'manual')),
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),
    created_by              uuid REFERENCES "user" (id),
    deleted_at              timestamptz
);

CREATE INDEX file_arrangement_id_idx ON file (arrangement_id);
CREATE INDEX file_voice_id_idx ON file (voice_id);
CREATE INDEX file_derived_from_file_id_idx ON file (derived_from_file_id);

-- Unique constraints table (CLAUDE.md): two partial indexes, since
-- Postgres treats NULL as distinct in a unique index and a single index
-- over (arrangement_id, voice_id, name, format) would not collapse
-- multiple full-score rows (voice_id IS NULL) the way we want compared
-- against each other:
--   - voice files:      (arrangement_id, voice_id, name, format)
--   - full-score files: (arrangement_id, name, format) WHERE voice_id IS NULL
-- Both restricted to live rows only.
CREATE UNIQUE INDEX file_voice_file_key
    ON file (arrangement_id, voice_id, name, format)
    WHERE deleted_at IS NULL AND voice_id IS NOT NULL;

CREATE UNIQUE INDEX file_score_file_key
    ON file (arrangement_id, name, format)
    WHERE deleted_at IS NULL AND voice_id IS NULL;
