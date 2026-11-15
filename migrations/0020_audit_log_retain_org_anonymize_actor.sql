-- Let `audit_log` rows outlive the org/user they reference, per CLAUDE.md.
--
-- The original 0018 schema declared `org_id` and `actor_user_id` as plain
-- `REFERENCES` columns with the default ON DELETE action (NO ACTION /
-- RESTRICT). Both Organization and User are hard-deleted in phase 1, and a
-- RESTRICT FK makes that impossible while any audit row references them — yet
-- the audit log is exactly the record that must survive the deletion.
--
-- Two columns, two different intents (CLAUDE.md spells them out separately):
--
--   * `org_id` — Organization entity, "Not cascaded": audit_log rows are
--     "retained with their `org_id` so the deletion itself stays forensically
--     auditable after the org is gone". So the org_id VALUE must persist
--     verbatim after the org row is deleted. ON DELETE SET NULL would destroy
--     exactly that, and worse: the audit design already uses `org_id IS NULL`
--     to mean "instance-wide event" (Work/Instrument writes), so nulling a
--     deleted org's rows would make them indistinguishable from instance-wide
--     events. The correct shape for an append-only forensic log is therefore
--     NO foreign key at all — `org_id` becomes a plain retained uuid (its
--     index from 0018 stays, so lookups by org are still fast).
--
--   * `actor_user_id` — User entity: user deletion is "admin-gated hard delete
--     with explicit reassignment-or-anonymization of ... audit log entries".
--     Anonymization == nulling the actor. So here ON DELETE SET NULL IS the
--     intended behavior; the column is already documented nullable (system
--     events). Keep the FK, switch it to SET NULL.

-- org_id: drop the FK entirely; the value is retained as a plain uuid.
ALTER TABLE audit_log
    DROP CONSTRAINT audit_log_org_id_fkey;

-- actor_user_id: keep the FK but anonymize (NULL) on user hard-delete.
ALTER TABLE audit_log
    DROP CONSTRAINT audit_log_actor_user_id_fkey,
    ADD CONSTRAINT audit_log_actor_user_id_fkey
        FOREIGN KEY (actor_user_id) REFERENCES "user" (id) ON DELETE SET NULL;
