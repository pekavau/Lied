-- PartAssignment: which user plays which voice for a specific item in a
-- collection (CLAUDE.md Entities: PartAssignment). Does not require the
-- user to hold a Membership in the org -- this is how guest/substitute
-- musicians are modeled. Not in the soft-delete entity list in CLAUDE.md;
-- reassignment replaces the row (per the Unique-constraints table note),
-- so no `deleted_at` here.
CREATE TABLE part_assignment (
    id                  uuid PRIMARY KEY,
    collection_item_id  uuid NOT NULL REFERENCES collection_item (id),
    user_id             uuid NOT NULL REFERENCES "user" (id),
    voice_id            uuid NOT NULL REFERENCES voice (id),
    notified_at         timestamptz,
    acknowledged_at     timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    created_by          uuid REFERENCES "user" (id)
);

CREATE INDEX part_assignment_collection_item_id_idx ON part_assignment (collection_item_id);
CREATE INDEX part_assignment_user_id_idx ON part_assignment (user_id);
CREATE INDEX part_assignment_voice_id_idx ON part_assignment (voice_id);

-- Unique constraints table (CLAUDE.md): PartAssignment (collection_item_id,
-- voice_id) -- one assignee per voice per item; reassignment replaces the
-- row. No soft-delete on this entity, so a plain unique index.
CREATE UNIQUE INDEX part_assignment_collection_item_id_voice_id_key
    ON part_assignment (collection_item_id, voice_id);
