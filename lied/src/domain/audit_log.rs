//! The single `audit()` write helper (CLAUDE.md Security baseline: Audit
//! logging) — "Phase 1 implementation is a single `audit(action, target,
//! payload)` helper called from every write site."
//!
//! Every write site passes a `serde_json::Value` payload; [`audit`] runs a
//! redaction pass over it before the `INSERT` so secrets never reach
//! storage, then writes the row. This is deliberately *not* part of the
//! same transaction as the write it documents in phase 1 — see the
//! `audit()` call sites in `auth` for the rationale (best-effort logging
//! must never block or roll back the auth operation it's describing).

use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// Static deny-list of JSON object keys whose value gets replaced with the
/// `"[redacted]"` sentinel before write (CLAUDE.md Redaction policy). The
/// key is retained so a diff still shows *that* the field changed, just not
/// its value. Applied recursively since payloads can nest (e.g. a
/// before/after diff object).
const REDACTED_KEYS: &[&str] = &[
    "password_hash",
    "passwordHash",
    "hash",
    "token",
    "plaintext",
    "signing_key",
    "signingKey",
    "client_secret",
    "clientSecret",
];

/// Recursively redact any object key in [`REDACTED_KEYS`], in place.
fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if REDACTED_KEYS.contains(&key.as_str()) {
                    *val = Value::String("[redacted]".to_string());
                } else {
                    redact(val);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact(item);
            }
        }
        _ => {}
    }
}

/// Context identifying *who* did *what* — passed by every call site. Kept
/// as a single struct rather than positional args since most fields are
/// optional and call sites otherwise drift on argument order.
#[derive(Debug, Clone, Default)]
pub struct AuditContext {
    pub actor_user_id: Option<Uuid>,
    pub org_id: Option<Uuid>,
    pub request_id: Option<Uuid>,
}

/// Write one audit_log row. `action` is a stable dotted string (e.g.
/// `"auth.login_failed"`); `target_kind`/`target_id` identify what was
/// acted on; `payload` is arbitrary JSON context, redacted before storage.
///
/// Errors are logged but never propagated — a failure to write an audit row
/// must not fail the auth operation it documents (CLAUDE.md's audit rule is
/// "every write is logged", not "every write requires its log to succeed");
/// an audit outage shouldn't become a login outage.
pub async fn audit(
    pool: &PgPool,
    ctx: &AuditContext,
    action: &str,
    target_kind: &str,
    target_id: Option<Uuid>,
    mut payload: Value,
) {
    redact(&mut payload);

    let result = sqlx::query!(
        r#"
        INSERT INTO audit_log (id, actor_user_id, org_id, action, target_kind, target_id, payload, request_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
        Uuid::now_v7(),
        ctx.actor_user_id,
        ctx.org_id,
        action,
        target_kind,
        target_id,
        payload,
        ctx.request_id,
    )
    .execute(pool)
    .await;

    if let Err(error) = result {
        tracing::error!(%error, action, target_kind, "failed to write audit log row");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redact_replaces_denylisted_keys() {
        let mut payload = json!({
            "username": "alice",
            "password_hash": "argon2-secret-blob",
        });
        redact(&mut payload);
        assert_eq!(payload["username"], "alice");
        assert_eq!(payload["password_hash"], "[redacted]");
    }

    #[test]
    fn redact_recurses_into_nested_objects() {
        let mut payload = json!({
            "diff": {
                "before": { "hash": "old-hash" },
                "after": { "hash": "new-hash" }
            }
        });
        redact(&mut payload);
        assert_eq!(payload["diff"]["before"]["hash"], "[redacted]");
        assert_eq!(payload["diff"]["after"]["hash"], "[redacted]");
    }

    #[test]
    fn redact_recurses_into_arrays() {
        let mut payload = json!({
            "tokens": [
                { "plaintext": "lied_aaa" },
                { "plaintext": "lied_bbb" }
            ]
        });
        redact(&mut payload);
        assert_eq!(payload["tokens"][0]["plaintext"], "[redacted]");
        assert_eq!(payload["tokens"][1]["plaintext"], "[redacted]");
    }

    #[test]
    fn redact_leaves_non_denylisted_keys_alone() {
        let mut payload = json!({ "action": "login", "success": true });
        redact(&mut payload);
        assert_eq!(payload["action"], "login");
        assert_eq!(payload["success"], true);
    }
}
