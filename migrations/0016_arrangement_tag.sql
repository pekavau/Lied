-- ArrangementTag: many-to-many join between Arrangement and Tag (CLAUDE.md
-- Entities: ArrangementTag). No soft-delete on the join itself.
CREATE TABLE arrangement_tag (
    id              uuid PRIMARY KEY,
    arrangement_id  uuid NOT NULL REFERENCES arrangement (id),
    tag_id          uuid NOT NULL REFERENCES tag (id)
);

CREATE INDEX arrangement_tag_arrangement_id_idx ON arrangement_tag (arrangement_id);
CREATE INDEX arrangement_tag_tag_id_idx ON arrangement_tag (tag_id);

-- Unique constraints table (CLAUDE.md): ArrangementTag (arrangement_id,
-- tag_id). No soft-delete on this entity.
CREATE UNIQUE INDEX arrangement_tag_arrangement_id_tag_id_key
    ON arrangement_tag (arrangement_id, tag_id);
