-- Instrument: instance-wide controlled vocabulary (docs/design.md Entities:
-- Instrument). Seeded with ~150 standard instruments in
-- 0019_seed_instruments.sql. No `deleted_at` in phase 1 -- referenced by
-- Voice/Membership and never hard-deleted (see docs/design.md Decisions).
CREATE TABLE instrument (
    id              uuid PRIMARY KEY,
    key             text NOT NULL,
    display_name    text NOT NULL,
    aliases         text[] NOT NULL DEFAULT '{}',
    family          text NOT NULL
        CHECK (family IN ('brass', 'woodwind', 'strings', 'percussion', 'keyboard', 'voice', 'other')),
    transposition   text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    created_by      uuid
);

-- Unique constraints table (docs/design.md): Instrument.key. No soft-delete on
-- this entity, so a plain (non-partial) unique index is correct.
CREATE UNIQUE INDEX instrument_key_key ON instrument (key);
