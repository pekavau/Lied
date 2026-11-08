-- Voice.part_number: which part of its instrument a voice is (issue #55).
--
-- "1st trumpet" used to live only inside the voice name ("Trumpet in Bb 1",
-- "Violin I", "1. Trompete"), so two pieces' first-trumpet parts were related by
-- nothing the model could name. `(instrument_id, part_number)` is now the
-- identity of "the same part across pieces", which is what assigning one player
-- to a voice across a whole collection matches on.
--
-- NULL is an ordinary value, not a gap: plenty of parts have no number
-- (Timpani, Harp, Piano, Solo Cornet), and bulk assignment matches NULL to NULL.
ALTER TABLE voice
    ADD COLUMN part_number smallint CHECK (part_number >= 1);

-- Best-effort guess of a part number from a voice name, used to backfill the
-- voices that existed before the column did. Deliberately conservative: it only
-- answers when the name carries exactly one ordinal in an unmistakable place,
-- and returns NULL for anything else -- a wrong number silently hands a player
-- the wrong part, whereas a missing one shows up in the bulk-assign preview as
-- "no matching voice", where the archivist can see it and fix it.
--
-- Recognised:
--   trailing arabic   "Trumpet in Bb 1", "Horn in F 12"
--   trailing roman    "Violin I" .. "Violin VIII" (upper case only)
--   leading ordinal   "1st Trumpet", "2nd Violin", "2. Trompete"
-- Rejected (NULL):
--   any separator that signals a combined or doubling part (/ & + ,):
--     "Horn 1 & 3", "Flute 2/Piccolo", "Trombone 1+2"
--   any second number: "Trumpet 1 in 3/4" etc.
--   0 or three-digit numbers.
--
-- IMMUTABLE: it depends on its argument alone, and declaring it so lets a test
-- call it directly to pin the heuristic.
CREATE FUNCTION lied_guess_part_number(name text) RETURNS smallint
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
AS $$
    SELECT CASE
        WHEN btrim(name) ~ '^[^0-9/&+,]*\s[1-9][0-9]?$'
            THEN substring(btrim(name) FROM '([1-9][0-9]?)$')::smallint
        WHEN btrim(name) ~ '^[^0-9/&+,]*\s(I|II|III|IV|V|VI|VII|VIII)$'
            THEN CASE substring(btrim(name) FROM '\s(I|II|III|IV|V|VI|VII|VIII)$')
                WHEN 'I' THEN 1
                WHEN 'II' THEN 2
                WHEN 'III' THEN 3
                WHEN 'IV' THEN 4
                WHEN 'V' THEN 5
                WHEN 'VI' THEN 6
                WHEN 'VII' THEN 7
                WHEN 'VIII' THEN 8
            END::smallint
        WHEN btrim(name) ~ '^[1-9][0-9]?(st|nd|rd|th|\.)\s+[^0-9/&+,]+$'
            THEN substring(btrim(name) FROM '^([1-9][0-9]?)')::smallint
    END
$$;

-- Soft-deleted voices are backfilled too: undeleting one should bring back a
-- voice that behaves like its live siblings.
UPDATE voice SET part_number = lied_guess_part_number(name);

-- Bulk assignment looks up "the voices of this instrument" within each piece of
-- a collection; the existing (instrument_id) index already serves that, and the
-- per-arrangement fan-out is small, so no new index is added.
