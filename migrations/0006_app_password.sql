-- AppPassword: revocable, WebDAV-scoped credential issued per device
-- (CLAUDE.md Entities: AppPassword). No `expires_at` in phase 1; a nullable
-- expiry column can be added later without a backfill (null = never
-- expires).
CREATE TABLE app_password (
    id            uuid PRIMARY KEY,
    user_id       uuid NOT NULL REFERENCES "user" (id),
    name          text NOT NULL,
    hash          text NOT NULL,
    prefix        text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    last_used_at  timestamptz,
    revoked_at    timestamptz
);

CREATE INDEX app_password_user_id_idx ON app_password (user_id);
-- Fast lookup path for WebDAV Basic auth: filter by user + prefix among
-- still-active passwords before the (intentionally slow) argon2 verify.
CREATE INDEX app_password_prefix_idx ON app_password (user_id, prefix) WHERE revoked_at IS NULL;

-- Unique constraints table (CLAUDE.md): AppPassword (user_id, name). No
-- soft-delete on this entity (revocation is `revoked_at`, not deletion), so
-- this is a plain unique index over the full row set.
CREATE UNIQUE INDEX app_password_user_id_name_key ON app_password (user_id, name);
