-- Organization: an orchestra or ensemble (CLAUDE.md Entities: Organization).
-- No `deleted_at` -- deleting an org is an admin-gated hard delete (cascade
-- impact is large; undelete UX is messy). `slug` is immutable per the
-- WebDAV-layout decision; renames are a separate, explicit, audited op.
CREATE TABLE organization (
    id          uuid PRIMARY KEY,
    name        text NOT NULL,
    slug        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    created_by  uuid
);

-- Unique constraints table (CLAUDE.md): Organization.slug, global.
CREATE UNIQUE INDEX organization_slug_key ON organization (slug);
