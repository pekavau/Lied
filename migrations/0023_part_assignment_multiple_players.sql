-- A part is played by several musicians (issue #53).
--
-- The original unique index -- (collection_item_id, voice_id) -- allowed one
-- assignee per voice per piece, and `assign` was an upsert that replaced
-- whoever was there. That models a chamber group: in an orchestra every desk of
-- the second violins reads the same part, so assigning the second player
-- silently removed the first.
--
-- The same PERSON twice on one part is still a mistake, so the uniqueness moves
-- rather than disappearing. Existing rows already satisfy the new index (they
-- were unique on a strict prefix of it), so there is nothing to backfill or
-- deduplicate.
DROP INDEX part_assignment_collection_item_id_voice_id_key;

CREATE UNIQUE INDEX part_assignment_collection_item_id_voice_id_user_id_key
    ON part_assignment (collection_item_id, voice_id, user_id);
