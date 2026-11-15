-- Session store table for `tower-sessions-sqlx-store`'s `PostgresStore`.
--
-- Reproduces exactly the DDL that `PostgresStore::migrate()` would otherwise
-- run at startup (see `tower_sessions_sqlx_store::postgres_store`), so that
-- every schema object lives in our own numbered `/migrations` set instead of
-- being created out-of-band by the crate at runtime.
--
-- Per CLAUDE.md's infra-pluggability rule, this table is treated as an
-- opaque key/value store owned by the `tower-sessions-sqlx-store` crate:
-- application code must never JOIN or add a foreign key against it. The
-- crate's own trait (`SessionStore`) is the only sanctioned access path.
CREATE SCHEMA IF NOT EXISTS tower_sessions;

CREATE TABLE IF NOT EXISTS tower_sessions.session (
    id          text PRIMARY KEY NOT NULL,
    data        bytea NOT NULL,
    expiry_date timestamptz NOT NULL
);
