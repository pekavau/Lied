-- One live entry per arrangement per collection.
--
-- Until now only `(collection_id, index)` was unique, so the same arrangement
-- could occupy two slots in one collection. The console's add-piece dropdown
-- filtered out pieces already present, but that was cosmetic: a crafted POST,
-- or restoring a removed piece after re-adding it, produced a duplicate that
-- nothing rejected — and a program listing the same piece twice is a data-entry
-- mistake, not a feature (an encore is a second CollectionItem in the *concert*
-- sense only if the archivist really wants it, which no one has asked for).
--
-- Live rows only, matching every other soft-delete unique index in the schema:
-- a removed piece keeps its row so it can be restored, and must not block
-- re-adding the same arrangement in the meantime.
CREATE UNIQUE INDEX collection_item_collection_id_arrangement_id_key
    ON collection_item (collection_id, arrangement_id)
    WHERE deleted_at IS NULL;
