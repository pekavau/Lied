-- A voice can carry several instruments (#57): a percussion part read by one
-- player ("Perc. 1: snare, bass drum, cymbals") or a doubling ("Flute 2 /
-- Piccolo"). `voice.instrument_id` becomes the join table `voice_instrument`;
-- `position` orders a voice's instruments, and position 1 is the primary,
-- which display, sorting and part identity (bulk assignment, #55) use.
--
-- Rows are hard: a voice's list is replaced as a whole on every write. That a
-- voice has at least one instrument is checked by the domain, in the same
-- transaction as the voice write (a deferred constraint trigger could, but
-- every write already goes through `domain::voice`).

CREATE TABLE voice_instrument (
    voice_id      uuid NOT NULL REFERENCES voice (id),
    instrument_id uuid NOT NULL REFERENCES instrument (id),
    position      smallint NOT NULL CHECK (position >= 1),
    PRIMARY KEY (voice_id, instrument_id),
    CONSTRAINT voice_instrument_voice_id_position_key UNIQUE (voice_id, position)
);

-- "Which voices carry this instrument": the search facet, coverage's section
-- view and bulk assignment's primary-instrument match.
CREATE INDEX voice_instrument_instrument_id_idx ON voice_instrument (instrument_id);

INSERT INTO voice_instrument (voice_id, instrument_id, position)
SELECT id, instrument_id, 1 FROM voice;

DROP INDEX voice_instrument_id_idx;
ALTER TABLE voice DROP COLUMN instrument_id;
