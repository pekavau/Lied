-- Collection numbering (#102; docs/design.md, Data Model → *Collection
-- numbering*). A piece's place in a collection becomes two values:
--
-- - `position`: the running order, dense 1..n over live items in both
--   collection types, maintained by the server (the old `index` column);
-- - `label`: the number players are called by. In a program it is always the
--   position as text; in a standing collection (Marschbuch,
--   Frühschoppenmappe) it is the piece's identity, matching the paper copy,
--   and keeps its gaps until an explicit renumber.
--
-- And a collection gets `number_width`: the digits WebDAV pads labels to,
-- which also caps them (999 or 9999).

ALTER TABLE collection_item RENAME COLUMN index TO position;
-- Dropped before the backfill, which recomputes positions and would collide
-- with it mid-update; its replacement is added at the end.
DROP INDEX collection_item_collection_id_index_key;
ALTER TABLE collection_item ADD COLUMN label text;

ALTER TABLE collection
    ADD COLUMN number_width smallint NOT NULL DEFAULT 3
        CHECK (number_width IN (3, 4));

-- Backfill. Since commit 51949a1 live numbering is dense, so for live items the
-- old index is both the position and the label. Two guards keep the
-- backfill total even for data written before that (the API allowed any
-- index up to 999,999 until #102):
--   * live positions are recomputed densely from the old order;
--   * a live label above 9999 (which the format cannot hold) is replaced by
--     the next numbers after the collection's highest legal one, in order.
-- A program's labels are its positions.
WITH live AS (
    SELECT ci.id, ci.collection_id, ci.position AS old_index, c.type,
           row_number() OVER (PARTITION BY ci.collection_id ORDER BY ci.position, ci.id) AS pos
    FROM collection_item ci
    JOIN collection c ON c.id = ci.collection_id
    WHERE ci.deleted_at IS NULL
),
legal_max AS (
    SELECT collection_id, coalesce(max(old_index) FILTER (WHERE old_index <= 9999), 0) AS top
    FROM live GROUP BY collection_id
),
oversize AS (
    SELECT l.id,
           m.top + row_number() OVER (PARTITION BY l.collection_id ORDER BY l.old_index, l.id) AS label_number
    FROM live l JOIN legal_max m USING (collection_id)
    WHERE l.type = 'standing' AND l.old_index > 9999
)
UPDATE collection_item ci
SET position = live.pos,
    label = CASE
        WHEN live.type = 'program' THEN live.pos::text
        WHEN live.old_index > 9999 THEN (SELECT o.label_number FROM oversize o WHERE o.id = live.id)::text
        ELSE live.old_index::text
    END
FROM live
WHERE ci.id = live.id;

-- Removed items keep their old index as position (unconstrained) and a
-- label in range, so a restore has a number to try first.
UPDATE collection_item
SET label = least(greatest(position, 1), 9999)::text
WHERE deleted_at IS NOT NULL;

-- Collections whose labels now pass 999 need the wider folder names.
UPDATE collection c
SET number_width = 4
WHERE EXISTS (
    SELECT 1 FROM collection_item ci
    WHERE ci.collection_id = c.id AND ci.deleted_at IS NULL
      AND (regexp_replace(ci.label, '[a-z]$', ''))::int > 999
);

ALTER TABLE collection_item
    ALTER COLUMN label SET NOT NULL,
    ADD CONSTRAINT collection_item_label_format
        CHECK (label ~ '^[1-9][0-9]{0,3}[a-z]?$');

-- Uniqueness among live items, deferred to commit. A swap, a renumber or an
-- insertion moves several items through each other's position or label in
-- one transaction; with an immediate unique index those intermediate states
-- would collide (and labels, unlike integers, have no free range to park
-- in). Every writer serializes on the collection row first, so these
-- constraints are a backstop, not the mechanism. Exclusion constraints are
-- used because a partial unique *index* cannot be deferred.
ALTER TABLE collection_item
    ADD CONSTRAINT collection_item_live_position_key
        EXCLUDE USING btree (collection_id WITH =, position WITH =)
        WHERE (deleted_at IS NULL)
        DEFERRABLE INITIALLY DEFERRED,
    ADD CONSTRAINT collection_item_live_label_key
        EXCLUDE USING btree (collection_id WITH =, label WITH =)
        WHERE (deleted_at IS NULL)
        DEFERRABLE INITIALLY DEFERRED;
