-- Sections of a standing collection (#103; docs/design.md, Data Model →
-- *CollectionSection* and *Collection numbering*): a heading over a range of
-- numbers ("Märsche" from 1, "Polkas" from 61, "Modern" from 101). A section
-- runs from its `starts_at` to the next section's start. Items do not point
-- at a section; which one a piece is in follows from its number.
--
-- Hard-deleted (no `deleted_at`): deleting a section only removes a heading,
-- no number changes, and the pieces fall into the section before it.

CREATE TABLE collection_section (
    id              uuid PRIMARY KEY,
    collection_id   uuid NOT NULL REFERENCES collection (id),
    name            text NOT NULL,
    starts_at       integer NOT NULL CHECK (starts_at BETWEEN 1 AND 9999),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    created_by      uuid REFERENCES "user" (id),
    -- One section per start. Deferred to commit, like the item numbering
    -- constraints of 0026: a renumber moves every section's start in one
    -- transaction, and a start can pass through another's old value (letter
    -- labels make numbers grow: 1, 1a, 2 → 1, 2, 3). Every writer holds the
    -- collection lock first, so this is a backstop, not the mechanism.
    CONSTRAINT collection_section_start_key
        UNIQUE (collection_id, starts_at) DEFERRABLE INITIALLY DEFERRED
);
