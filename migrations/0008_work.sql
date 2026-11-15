-- Work: the abstract musical work (CLAUDE.md Entities: Work). Instance-wide
-- -- no `organization_id`, shared across orgs (a feature in the
-- federation topology). No `deleted_at` either: Works are durable
-- references, orphans are not garbage-collected. No unique constraint --
-- duplicates allowed in phase 1; resolved via phase-2 merge tooling.
-- `created_by` doubles as the edit-permission anchor (CLAUDE.md Decisions:
-- editing a Work is restricted to its creator or a system admin).
CREATE TABLE work (
    id          uuid PRIMARY KEY,
    title       text NOT NULL,
    composer    text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    created_by  uuid REFERENCES "user" (id)
);
