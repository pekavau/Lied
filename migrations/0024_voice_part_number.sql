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

-- Best-effort guess of a part number from a voice name. Used twice: to
-- backfill the voices that existed before the column did, and by the
-- application as the default for a new voice created without an explicit
-- part number -- one implementation, so the two cannot disagree.
--
-- Deliberately conservative. It answers only when the name carries exactly
-- ONE ordinal, in an unmistakable place, and returns NULL for anything else:
-- a wrong number silently hands a player the wrong part, whereas a missing one
-- shows up in the bulk-assign preview, where the archivist can see and fix it.
--
-- The name is split on whitespace; a token is an ordinal if it is arabic
-- digits (optionally "1st"/"2nd"/"2."), or an upper-case roman numeral I..VIII.
--
-- Recognised (exactly one ordinal):
--   trailing arabic   "Trumpet in Bb 1", "Horn in F 12"
--   trailing roman    "Violin I" .. "Viola VIII"
--   leading ordinal   "1st Trumpet", "2nd Violin", "2. Trompete"
-- NULL:
--   two or more ordinals, however they are joined -- combined parts:
--     "Horn 1 und 3", "Violin I und II", "Violin I - II", "1st Violin II"
--   the separators / & + , anywhere: "Flute 2/Piccolo", "Trombone 1+2"
--   0, three-digit numbers, or an ordinal in the middle of the name
--   lower-case roman ("Violin ii"): too easily an ordinary word
--
-- IMMUTABLE: it depends on its argument alone, which lets tests call it
-- directly to pin the heuristic.
CREATE FUNCTION lied_guess_part_number(name text) RETURNS smallint
    LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
AS $$
DECLARE
    romans constant text[] := ARRAY['I', 'II', 'III', 'IV', 'V', 'VI', 'VII', 'VIII'];
    tokens text[];
    token text;
    ordinals int := 0;
    last text;
    first text;
BEGIN
    IF name ~ '[/&+,]' THEN
        RETURN NULL;
    END IF;
    tokens := array_remove(regexp_split_to_array(btrim(name), '\s+'), '');
    IF coalesce(array_length(tokens, 1), 0) < 2 THEN
        RETURN NULL;
    END IF;

    FOREACH token IN ARRAY tokens LOOP
        IF token ~ '^[0-9]+(st|nd|rd|th|\.)?$' OR token = ANY (romans) THEN
            ordinals := ordinals + 1;
        END IF;
    END LOOP;
    IF ordinals <> 1 THEN
        RETURN NULL;
    END IF;

    last := tokens[array_length(tokens, 1)];
    IF last ~ '^[1-9][0-9]?$' THEN
        RETURN last::smallint;
    END IF;
    IF last = ANY (romans) THEN
        RETURN array_position(romans, last)::smallint;
    END IF;

    first := tokens[1];
    IF first ~ '^[1-9][0-9]?(st|nd|rd|th|\.)$' THEN
        RETURN substring(first FROM '^[0-9]+')::smallint;
    END IF;
    RETURN NULL;
END
$$;

-- Soft-deleted voices are backfilled too: undeleting one should bring back a
-- voice that behaves like its live siblings.
UPDATE voice SET part_number = lied_guess_part_number(name);

-- Bulk assignment looks up "the voices of this instrument" within each piece of
-- a collection; the existing (instrument_id) index already serves that, and the
-- per-arrangement fan-out is small, so no new index is added.
