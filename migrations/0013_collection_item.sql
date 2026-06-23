-- CollectionItem: an arrangement within a collection with its local index
-- number (CLAUDE.md Entities: CollectionItem).
CREATE TABLE collection_item (
    id              uuid PRIMARY KEY,
    collection_id   uuid NOT NULL REFERENCES collection (id),
    arrangement_id  uuid NOT NULL REFERENCES arrangement (id),
    index           integer NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    created_by      uuid REFERENCES "user" (id),
    deleted_at      timestamptz
);

CREATE INDEX collection_item_collection_id_idx ON collection_item (collection_id);
CREATE INDEX collection_item_arrangement_id_idx ON collection_item (arrangement_id);

-- Unique constraints table (CLAUDE.md): CollectionItem (collection_id,
-- index), live rows only.
CREATE UNIQUE INDEX collection_item_collection_id_index_key
    ON collection_item (collection_id, index)
    WHERE deleted_at IS NULL;
