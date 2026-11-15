-- audit_log: shallow, append-only-by-convention audit trail (CLAUDE.md
-- Security baseline: Audit logging). `org_id` is nullable for
-- instance-level events and for writes to instance-wide entities (Work,
-- Instrument), which are never attributed to an org regardless of which
-- org the actor was operating from. `actor_user_id` is nullable for system
-- events. `payload` is jsonb with secrets redacted before write (see
-- CLAUDE.md Redaction policy) -- the redaction happens in the `audit()`
-- application-layer helper, not in SQL.
CREATE TABLE audit_log (
    id              uuid PRIMARY KEY,
    at              timestamptz NOT NULL DEFAULT now(),
    actor_user_id   uuid REFERENCES "user" (id),
    org_id          uuid REFERENCES organization (id),
    action          text NOT NULL,
    target_kind     text NOT NULL,
    target_id       uuid,
    payload         jsonb,
    request_id      uuid
);

CREATE INDEX audit_log_at_idx ON audit_log (at);
CREATE INDEX audit_log_actor_user_id_idx ON audit_log (actor_user_id);
CREATE INDEX audit_log_org_id_idx ON audit_log (org_id);
CREATE INDEX audit_log_target_kind_target_id_idx ON audit_log (target_kind, target_id);
