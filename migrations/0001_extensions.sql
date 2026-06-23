-- Extensions required by later migrations.
--
-- citext: case-insensitive text, used for User.username and User.email
-- (CLAUDE.md Entities: User).
--
-- pgcrypto: provides gen_random_uuid(), used only for the static instrument
-- seed rows in 0019_seed_instruments.sql. Runtime application code always
-- generates UUIDv7 PKs app-side via the `uuid` crate (see CLAUDE.md
-- Implementation conventions); gen_random_uuid() produces a v4 UUID, which is
-- acceptable for the seed because the instrument seed is a one-time, static,
-- non-time-ordered batch insert -- there is no insert-locality benefit to
-- chase for a table that is written once at migration time and essentially
-- read-only afterward.
CREATE EXTENSION IF NOT EXISTS citext;
CREATE EXTENSION IF NOT EXISTS pgcrypto;
