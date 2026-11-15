-- GlobalAnnotation: a conductor-/director-level annotation on an
-- arrangement, visible to all (CLAUDE.md Entities: GlobalAnnotation). Not
-- in the soft-delete entity list, so no `deleted_at`. No unique constraint
-- -- duplicates are the norm (multiple annotations per arrangement).
CREATE TABLE global_annotation (
    id              uuid PRIMARY KEY,
    arrangement_id  uuid NOT NULL REFERENCES arrangement (id),
    author_id       uuid NOT NULL REFERENCES "user" (id),
    type            text NOT NULL,
    content         text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    created_by      uuid REFERENCES "user" (id)
);

CREATE INDEX global_annotation_arrangement_id_idx ON global_annotation (arrangement_id);
CREATE INDEX global_annotation_author_id_idx ON global_annotation (author_id);
