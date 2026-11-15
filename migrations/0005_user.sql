-- User: a person using the system (CLAUDE.md Entities: User). No
-- system-wide application role -- authorization is per-org via
-- Membership.role; `is_system_admin` covers instance-level operations only.
-- No `deleted_at` in phase 1 -- user deletion is admin-gated hard delete
-- with explicit reassignment/anonymization, a phase-2 concern.
CREATE TABLE "user" (
    id              uuid PRIMARY KEY,
    slug            text NOT NULL,
    username        citext NOT NULL,
    email           citext,
    password_hash   text,
    display_name    text NOT NULL,
    is_system_admin boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    created_by      uuid
);

-- Unique constraints table (CLAUDE.md): User.slug; User.username;
-- User.email (partial, where not null -- many users may have no email).
CREATE UNIQUE INDEX user_slug_key ON "user" (slug);
CREATE UNIQUE INDEX user_username_key ON "user" (username);
CREATE UNIQUE INDEX user_email_key ON "user" (email) WHERE email IS NOT NULL;
