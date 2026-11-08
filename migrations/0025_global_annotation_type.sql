-- GlobalAnnotation.type becomes a controlled vocabulary (issue #36).
--
-- 0017 created `type` as free text. CLAUDE.md's enum convention stores every
-- status/type/kind column as text + CHECK (only Tag.kind is free text, by
-- design), so the vocabulary is fixed here, before any client writes to it:
--
--   tempo         tempo changes, fermatas, rit./accel. decisions
--   dynamics      dynamic shaping, balance
--   articulation  bowings, phrasing, breath marks for the whole ensemble
--   structure     cuts, repeats, vamps, D.S./coda decisions
--   general       anything else
--
-- Adding a value later is a one-line migration; removing the CHECK entirely is
-- easier still. The table had no writer before this issue, but any row a test
-- or a hand edit left behind is mapped to 'general' rather than failing the
-- migration.
UPDATE global_annotation
SET type = 'general'
WHERE type NOT IN ('tempo', 'dynamics', 'articulation', 'structure', 'general');

ALTER TABLE global_annotation
    ADD CONSTRAINT global_annotation_type_check
    CHECK (type IN ('tempo', 'dynamics', 'articulation', 'structure', 'general'));

-- Listing an arrangement's annotations orders by creation; the existing
-- (arrangement_id) index already narrows to a handful of rows per piece, so no
-- composite index is added.
