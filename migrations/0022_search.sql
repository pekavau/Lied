-- Archive search (issue #34): full-text over the arrangement and the text that
-- hangs off it, plus trigram fuzzy matching on title and composer.
--
-- CLAUDE.md (Data Model -> Search & metadata) specifies FTS over title,
-- composer, arranger, instrumentation description and tag names, with pg_trgm
-- for fuzzy title/composer matching. The searchable text therefore spans THREE
-- tables -- `arrangement`, `work` (composer) and `tag` (via `arrangement_tag`)
-- -- which is why the vector cannot be a generated column: a generated column
-- may only read its own row. The two real options were:
--
--   (a) a stored `tsvector` column kept current by triggers on every
--       contributing table, or
--   (b) building the document at query time from a join.
--
-- (b) is correct by construction but forfeits the index -- every search would
-- be a sequential scan building tsvectors for the whole org -- so this
-- migration takes (a). The cost is the trigger set below; the invariant it has
-- to hold is that renaming a Work's composer or a Tag reindexes the
-- arrangements that reference it, which the tests exercise directly.
--
-- Text search configuration: 'simple' (no stemming, no stopwords) over
-- unaccented text. The catalogue is deliberately multilingual -- Boléro, Eine
-- kleine Nachtmusik, Oktoberfest Polka Medley -- and Postgres applies one
-- configuration to the whole column, so any stemmer would be wrong for most of
-- the corpus. Unaccenting means `Bolero` finds `Boléro` and `Dvorak` finds
-- `Dvořák`; the fuzziness a stemmer would have provided comes from the trigram
-- indexes instead.

CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS unaccent;

-- `unaccent()` is only STABLE (its behaviour depends on a dictionary that can
-- be replaced), so PostgreSQL refuses it in an index expression. This wrapper
-- asserts immutability so the trigram indexes below can be built on unaccented
-- text.
--
-- CAVEAT: if the unaccent dictionary is ever modified, indexes built on this
-- function must be REINDEXed -- the assertion is a promise the operator keeps,
-- not one PostgreSQL can check. The dictionary is stock and we do not ship a
-- replacement, so this is a theoretical concern documented rather than guarded.
CREATE OR REPLACE FUNCTION immutable_unaccent(text)
RETURNS text
LANGUAGE sql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $$
    SELECT unaccent('unaccent', $1)
$$;

ALTER TABLE arrangement ADD COLUMN search_vector tsvector;

-- The weighted search document for one arrangement.
--
-- Weights drive `ts_rank`, so a query word matching a title outranks the same
-- word buried in an instrumentation note:
--   A  title, composer      -- what a piece IS
--   B  arranger, tag names  -- how it is classified
--   C  instrumentation      -- prose detail
CREATE OR REPLACE FUNCTION arrangement_search_document(arrangement_id uuid)
RETURNS tsvector
LANGUAGE sql
STABLE
AS $$
    SELECT
        setweight(to_tsvector('simple', immutable_unaccent(coalesce(a.title, ''))), 'A')
     || setweight(to_tsvector('simple', immutable_unaccent(coalesce(w.composer, ''))), 'A')
     || setweight(to_tsvector('simple', immutable_unaccent(coalesce(a.arranger, ''))), 'B')
     || setweight(
            to_tsvector(
                'simple',
                immutable_unaccent(
                    coalesce(
                        (
                            SELECT string_agg(t.name, ' ')
                            FROM arrangement_tag at
                            JOIN tag t ON t.id = at.tag_id
                            WHERE at.arrangement_id = a.id
                              AND t.deleted_at IS NULL
                        ),
                        ''
                    )
                )
            ),
            'B'
        )
     || setweight(to_tsvector('simple', immutable_unaccent(coalesce(a.instrumentation, ''))), 'C')
    FROM arrangement a
    LEFT JOIN work w ON w.id = a.work_id
    WHERE a.id = arrangement_id
$$;

-- Recompute one arrangement's vector. Used by every trigger below except the
-- arrangement's own BEFORE trigger, which can assign to NEW directly.
CREATE OR REPLACE FUNCTION refresh_arrangement_search_vector(target uuid)
RETURNS void
LANGUAGE sql
AS $$
    UPDATE arrangement
    SET search_vector = arrangement_search_document(target)
    WHERE id = target
$$;

-- 1. The arrangement's own columns.
--
-- AFTER, not BEFORE: `arrangement_search_document` reads the row back from the
-- table, so it must run once the new values are visible. The extra UPDATE is
-- confined to the row that just changed, and the WHEN clause keeps it off
-- writes that cannot affect the document (an index bump, a soft delete).
CREATE OR REPLACE FUNCTION arrangement_search_vector_trigger()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM refresh_arrangement_search_vector(NEW.id);
    RETURN NULL;
END;
$$;

CREATE TRIGGER arrangement_search_vector_insert
    AFTER INSERT ON arrangement
    FOR EACH ROW
    EXECUTE FUNCTION arrangement_search_vector_trigger();

CREATE TRIGGER arrangement_search_vector_update
    AFTER UPDATE OF title, arranger, instrumentation, work_id ON arrangement
    FOR EACH ROW
    WHEN (
        OLD.title IS DISTINCT FROM NEW.title
        OR OLD.arranger IS DISTINCT FROM NEW.arranger
        OR OLD.instrumentation IS DISTINCT FROM NEW.instrumentation
        OR OLD.work_id IS DISTINCT FROM NEW.work_id
    )
    EXECUTE FUNCTION arrangement_search_vector_trigger();

-- 2. The Work's composer/title: every arrangement pointing at it must follow.
CREATE OR REPLACE FUNCTION work_search_vector_trigger()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    UPDATE arrangement
    SET search_vector = arrangement_search_document(id)
    WHERE work_id = NEW.id;
    RETURN NULL;
END;
$$;

CREATE TRIGGER work_search_vector_update
    AFTER UPDATE OF title, composer ON work
    FOR EACH ROW
    WHEN (OLD.title IS DISTINCT FROM NEW.title OR OLD.composer IS DISTINCT FROM NEW.composer)
    EXECUTE FUNCTION work_search_vector_trigger();

-- 3. A Tag's name (or its soft-delete state): every arrangement carrying it.
CREATE OR REPLACE FUNCTION tag_search_vector_trigger()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    UPDATE arrangement
    SET search_vector = arrangement_search_document(arrangement.id)
    FROM arrangement_tag at
    WHERE at.arrangement_id = arrangement.id
      AND at.tag_id = NEW.id;
    RETURN NULL;
END;
$$;

CREATE TRIGGER tag_search_vector_update
    AFTER UPDATE OF name, deleted_at ON tag
    FOR EACH ROW
    WHEN (OLD.name IS DISTINCT FROM NEW.name OR OLD.deleted_at IS DISTINCT FROM NEW.deleted_at)
    EXECUTE FUNCTION tag_search_vector_trigger();

-- 4. Attaching or detaching a tag.
CREATE OR REPLACE FUNCTION arrangement_tag_search_vector_trigger()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM refresh_arrangement_search_vector(OLD.arrangement_id);
    ELSE
        PERFORM refresh_arrangement_search_vector(NEW.arrangement_id);
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER arrangement_tag_search_vector_change
    AFTER INSERT OR DELETE ON arrangement_tag
    FOR EACH ROW
    EXECUTE FUNCTION arrangement_tag_search_vector_trigger();

-- Backfill everything that predates the column.
UPDATE arrangement SET search_vector = arrangement_search_document(id);

-- Indexes.
--
-- GIN over the vector serves `@@`; the two trigram GINs serve the `%`
-- similarity operator that catches misspellings and partial words the FTS
-- tokeniser cannot ("bolerro", "nachtmus").
CREATE INDEX arrangement_search_vector_idx ON arrangement USING GIN (search_vector);

CREATE INDEX arrangement_title_trgm_idx
    ON arrangement USING GIN (immutable_unaccent(title) gin_trgm_ops);

CREATE INDEX work_composer_trgm_idx
    ON work USING GIN (immutable_unaccent(composer) gin_trgm_ops);
