//! Membership: a user's membership in an organization (CLAUDE.md Entities:
//! Membership). Issue #5 adds full CRUD, gated to an org's `owner` role
//! (CLAUDE.md Permission matrix: "Manage members & roles" — owner only).
//!
//! Two invariants enforced here rather than in SQL (CLAUDE.md Decisions:
//! "Postgres can't enforce element-level FKs on an array" /
//! "every org has at least one Membership with role = owner"):
//! 1. `instrument_ids` / `principal_instrument_ids` must reference live
//!    `Instrument` rows, validated in the service layer on write.
//!    `principal_instrument_ids` must be a subset of `instrument_ids`.
//! 2. An org must always keep >= 1 `owner`; demoting or removing the last
//!    owner is rejected.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// `Membership.role` (CLAUDE.md: text + CHECK, not a native PG enum — see
/// Implementation conventions). Ordered loosely by seniority for readability
/// only; the database is the source of truth for valid values via CHECK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Archivist,
    Conductor,
    Musician,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Archivist => "archivist",
            Role::Conductor => "conductor",
            Role::Musician => "musician",
        }
    }

    pub fn parse(input: &str) -> Option<Role> {
        match input {
            "owner" => Some(Role::Owner),
            "archivist" => Some(Role::Archivist),
            "conductor" => Some(Role::Conductor),
            "musician" => Some(Role::Musician),
            _ => None,
        }
    }

    // --- Permission matrix (CLAUDE.md "Permission matrix") ---
    //
    // The single source of truth for the org-role capability rows, so the
    // `/v1` authorization helpers ([`crate::auth::authz`]) and the `/admin`
    // console gate ([`crate::routes::admin::console`]) can't drift. These are
    // the *role* facts only; `is_system_admin` (owner-equivalent) and the
    // principal carve-out are context concerns layered on top by the callers.

    /// A staff role — owner, archivist, or conductor. Non-staff (musician)
    /// have no general console access.
    pub fn is_staff(self) -> bool {
        matches!(self, Role::Owner | Role::Archivist | Role::Conductor)
    }

    /// Upload/edit arrangements, voices, files, and tags.
    pub fn can_edit_arrangements(self) -> bool {
        matches!(self, Role::Owner | Role::Archivist)
    }

    /// Build and edit collections + manage part assignments.
    pub fn can_build_collections(self) -> bool {
        self.is_staff()
    }

    /// Manage members and roles, and org settings.
    pub fn can_manage_members(self) -> bool {
        matches!(self, Role::Owner)
    }

    /// Author global (conductor-level) annotations.
    pub fn can_author_global_annotations(self) -> bool {
        matches!(self, Role::Owner | Role::Conductor)
    }
}

/// Wire representation of a `membership` row. `camelCase` per CLAUDE.md's
/// JSON-casing convention.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Membership {
    pub id: Uuid,
    pub user_id: Uuid,
    pub organization_id: Uuid,
    pub role: Role,
    pub instrument_ids: Vec<Uuid>,
    pub is_principal: bool,
    pub principal_instrument_ids: Vec<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(thiserror::Error, Debug)]
pub enum MembershipError {
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("membership already exists for this user in this organization")]
    Duplicate,
    #[error("one or more instrument ids do not reference a live instrument")]
    UnknownInstrument,
    #[error("principal_instrument_ids must be a subset of instrument_ids")]
    PrincipalNotSubset,
    #[error("organization must keep at least one owner")]
    LastOwner,
}

struct Row {
    id: Uuid,
    user_id: Uuid,
    organization_id: Uuid,
    role: String,
    instrument_ids: Vec<Uuid>,
    is_principal: bool,
    principal_instrument_ids: Vec<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl Row {
    /// Convert a raw DB row into the wire type. `role` is guaranteed valid by
    /// the DB `CHECK` constraint, so an unparseable value indicates app/DB
    /// drift — falls back to `Musician` (the least-privileged role) rather
    /// than panicking, with a loud log line so the drift is noticed.
    fn into_membership(self) -> Membership {
        let role = Role::parse(&self.role).unwrap_or_else(|| {
            tracing::error!(role = %self.role, "unrecognized membership role in DB; defaulting to musician");
            Role::Musician
        });
        Membership {
            id: self.id,
            user_id: self.user_id,
            organization_id: self.organization_id,
            role,
            instrument_ids: self.instrument_ids,
            is_principal: self.is_principal,
            principal_instrument_ids: self.principal_instrument_ids,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

fn map_insert_error(err: sqlx::Error) -> MembershipError {
    if let sqlx::Error::Database(ref db_err) = err {
        if db_err.constraint() == Some("membership_user_id_organization_id_key") {
            return MembershipError::Duplicate;
        }
    }
    MembershipError::Database(err)
}

/// Validate that every id in `principal_instrument_ids` also appears in
/// `instrument_ids` (CLAUDE.md: "principal_instrument_ids ... subset of
/// instrument_ids").
fn principal_ids_are_subset(instrument_ids: &[Uuid], principal_instrument_ids: &[Uuid]) -> bool {
    principal_instrument_ids
        .iter()
        .all(|id| instrument_ids.contains(id))
}

/// Validate that every id in `ids` references a live `instrument` row
/// (CLAUDE.md: "validated against the live Instrument set in the service
/// layer on write" — Postgres can't enforce an element-level FK on a `uuid[]`
/// column). Empty input is trivially valid (no query needed).
async fn validate_instrument_ids(pool: &PgPool, ids: &[Uuid]) -> Result<bool, sqlx::Error> {
    if ids.is_empty() {
        return Ok(true);
    }
    let count: i64 =
        sqlx::query_scalar!(r#"SELECT count(*) FROM instrument WHERE id = ANY($1)"#, ids)
            .fetch_one(pool)
            .await?
            .unwrap_or(0);

    // Dedup `ids` before comparing, since a caller could list the same id
    // twice — count(*) over a deduped existence check should match the
    // distinct id count, not the raw input length.
    let mut distinct = ids.to_vec();
    distinct.sort();
    distinct.dedup();

    Ok(count == distinct.len() as i64)
}

/// Parameters for [`create`]/[`update`] — bundled since both take the same
/// mutable membership fields and positional args would otherwise drift.
pub struct MembershipFields<'a> {
    pub role: Role,
    pub instrument_ids: &'a [Uuid],
    pub is_principal: bool,
    pub principal_instrument_ids: &'a [Uuid],
}

/// Validate the cross-field/cross-table invariants shared by create/update:
/// instrument ids exist, and principal ids are a subset of instrument ids.
/// Does NOT check the last-owner invariant — that requires knowing the
/// *previous* role for updates, handled separately by callers that have it.
async fn validate_fields(
    pool: &PgPool,
    fields: &MembershipFields<'_>,
) -> Result<(), MembershipError> {
    if !principal_ids_are_subset(fields.instrument_ids, fields.principal_instrument_ids) {
        return Err(MembershipError::PrincipalNotSubset);
    }
    if !validate_instrument_ids(pool, fields.instrument_ids).await? {
        return Err(MembershipError::UnknownInstrument);
    }
    Ok(())
}

/// Insert a new membership row. Validates instrument ids and the
/// principal-subset invariant; does not need the last-owner check (creating
/// a membership can never remove an owner).
pub async fn create(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    organization_id: Uuid,
    fields: MembershipFields<'_>,
    created_by: Option<Uuid>,
) -> Result<Membership, MembershipError> {
    validate_fields(pool, &fields).await?;

    let row = sqlx::query_as!(
        Row,
        r#"
        INSERT INTO membership
            (id, user_id, organization_id, role, instrument_ids, is_principal, principal_instrument_ids, created_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING
            id, user_id, organization_id, role,
            instrument_ids, is_principal, principal_instrument_ids,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        "#,
        id,
        user_id,
        organization_id,
        fields.role.as_str(),
        fields.instrument_ids,
        fields.is_principal,
        fields.principal_instrument_ids,
        created_by,
    )
    .fetch_one(pool)
    .await
    .map_err(map_insert_error)?;

    Ok(row.into_membership())
}

/// Look up a membership by id.
pub async fn find_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Membership>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT
            id, user_id, organization_id, role,
            instrument_ids, is_principal, principal_instrument_ids,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        FROM membership
        WHERE id = $1
        "#,
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(Row::into_membership))
}

/// Look up a user's membership in a specific organization (the common
/// authorization-check shape: "does this user have a role in this org").
pub async fn find_by_user_and_org(
    pool: &PgPool,
    user_id: Uuid,
    organization_id: Uuid,
) -> Result<Option<Membership>, sqlx::Error> {
    let row = sqlx::query_as!(
        Row,
        r#"
        SELECT
            id, user_id, organization_id, role,
            instrument_ids, is_principal, principal_instrument_ids,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        FROM membership
        WHERE user_id = $1 AND organization_id = $2
        "#,
        user_id,
        organization_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(Row::into_membership))
}

/// Count how many *other* owners an org has, excluding `exclude_membership_id`
/// (the membership being updated/deleted) — used by the last-owner check.
async fn other_owner_count(
    pool: &PgPool,
    organization_id: Uuid,
    exclude_membership_id: Uuid,
) -> Result<i64, sqlx::Error> {
    Ok(sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM membership
        WHERE organization_id = $1 AND role = 'owner' AND id != $2
        "#,
        organization_id,
        exclude_membership_id,
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(0))
}

/// Sort allowlist for `GET /v1/orgs/{orgId}/members`. Default sort is
/// `createdAt:asc` (stable join order).
pub const SORT_ALLOWLIST: &[(&str, &str)] = &[
    ("role", "role"),
    ("isPrincipal", "is_principal"),
    ("createdAt", "created_at"),
];

/// Filter allowlist for `GET /v1/orgs/{orgId}/members`: exact match on
/// `role` (validated against the CHECK-enum values; an unrecognized role
/// value behaves as "matches nothing" rather than a SQL error, since the
/// column is `text` not a native enum).
pub const FILTER_ALLOWLIST: &[&str] = &["role"];

/// Fetch one page of memberships for an organization, with an allowlisted
/// sort and an optional exact-match `role` filter. See
/// [`crate::domain::organization::list`] for why building this query with
/// `format!` around fixed, allowlisted fragments is safe.
pub async fn list_for_org(
    pool: &PgPool,
    organization_id: Uuid,
    limit: i64,
    offset: i64,
    sort_column: &str,
    sort_direction: crate::listing::SortDirection,
    role_filter: Option<&str>,
) -> Result<(Vec<Membership>, i64), sqlx::Error> {
    let direction = sort_direction.as_sql();
    // Runtime `sqlx::query` (dynamic allowlisted ORDER BY): aliases must be
    // plain SQL identifiers, not the `"name!: Type"` cast-annotation syntax
    // (a `query!`-macro-only feature that Postgres would reject).
    let query = format!(
        r#"
        SELECT
            id, user_id, organization_id, role,
            instrument_ids, is_principal, principal_instrument_ids,
            created_at,
            updated_at,
            count(*) OVER() as total
        FROM membership
        WHERE organization_id = $3 AND ($4::text IS NULL OR role = $4)
        ORDER BY {sort_column} {direction}, id ASC
        LIMIT $1 OFFSET $2
        "#
    );

    let rows = sqlx::query(&query)
        .bind(limit)
        .bind(offset)
        .bind(organization_id)
        .bind(role_filter)
        .fetch_all(pool)
        .await?;

    use sqlx::Row as SqlxRow;
    let total = match rows.first() {
        Some(row) => row.try_get::<i64, _>("total")?,
        None => {
            let count_query = "SELECT count(*) FROM membership WHERE organization_id = $1 AND ($2::text IS NULL OR role = $2)";
            sqlx::query_scalar::<_, i64>(count_query)
                .bind(organization_id)
                .bind(role_filter)
                .fetch_one(pool)
                .await?
        }
    };

    let items = rows
        .into_iter()
        .map(|row| {
            let role_str: String = row.try_get("role")?;
            let role = Role::parse(&role_str).unwrap_or(Role::Musician);
            Ok(Membership {
                id: row.try_get("id")?,
                user_id: row.try_get("user_id")?,
                organization_id: row.try_get("organization_id")?,
                role,
                instrument_ids: row.try_get("instrument_ids")?,
                is_principal: row.try_get("is_principal")?,
                principal_instrument_ids: row.try_get("principal_instrument_ids")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    Ok((items, total))
}

/// Update an existing membership's mutable fields. Enforces the last-owner
/// invariant: if the membership currently holds `role = owner` and the new
/// `fields.role` is something else, the update is rejected unless at least
/// one *other* owner remains in the org.
///
/// Returns `Ok(None)` if no row matched `id`.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    fields: MembershipFields<'_>,
) -> Result<Option<Membership>, MembershipError> {
    validate_fields(pool, &fields).await?;

    let Some(current) = find_by_id(pool, id).await? else {
        return Ok(None);
    };

    if current.role == Role::Owner && fields.role != Role::Owner {
        let remaining = other_owner_count(pool, current.organization_id, id).await?;
        if remaining == 0 {
            return Err(MembershipError::LastOwner);
        }
    }

    let row = sqlx::query_as!(
        Row,
        r#"
        UPDATE membership
        SET role = $2, instrument_ids = $3, is_principal = $4, principal_instrument_ids = $5, updated_at = now()
        WHERE id = $1
        RETURNING
            id, user_id, organization_id, role,
            instrument_ids, is_principal, principal_instrument_ids,
            created_at as "created_at: DateTime<Utc>",
            updated_at as "updated_at: DateTime<Utc>"
        "#,
        id,
        fields.role.as_str(),
        fields.instrument_ids,
        fields.is_principal,
        fields.principal_instrument_ids,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(Row::into_membership))
}

/// Returns `true` if `user_id` holds at least one `Membership` in any
/// organization. Used to gate Work creation (CLAUDE.md Decisions: "creating
/// a Work is open to any authenticated user with at least one Membership in
/// any org").
pub async fn has_any_membership(pool: &PgPool, user_id: Uuid) -> Result<bool, sqlx::Error> {
    let exists: bool = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM membership WHERE user_id = $1) as "exists!""#,
        user_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// Delete a membership. Enforces the last-owner invariant: deleting an
/// `owner` membership is rejected unless at least one other owner remains.
///
/// Returns `Ok(true)` if a row was deleted, `Ok(false)` if `id` did not
/// exist, or `Err(MembershipError::LastOwner)` if the deletion would leave
/// the org without an owner.
pub async fn delete(pool: &PgPool, id: Uuid) -> Result<bool, MembershipError> {
    let Some(current) = find_by_id(pool, id).await? else {
        return Ok(false);
    };

    if current.role == Role::Owner {
        let remaining = other_owner_count(pool, current.organization_id, id).await?;
        if remaining == 0 {
            return Err(MembershipError::LastOwner);
        }
    }

    let result = sqlx::query!(r#"DELETE FROM membership WHERE id = $1"#, id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

/// One organization a user belongs to, with the role they hold there —
/// the rows the console's `/admin` landing lists so a member can enter each
/// org's workspace. Joins `membership` to `organization`; ordered by org
/// name for a stable, human-friendly list.
#[derive(Debug, Clone)]
pub struct UserOrg {
    pub organization_id: Uuid,
    pub organization_name: String,
    pub organization_slug: String,
    pub role: Role,
    pub is_principal: bool,
}

/// Every organization `user_id` is a member of, with their role in each.
/// Used by the console landing (`/admin`) to render the list of workspaces a
/// user can enter. A `is_system_admin` user additionally reaches every org
/// via [`crate::domain::organization::list`]; that superset is composed at
/// the call site, not here.
pub async fn list_for_user(pool: &PgPool, user_id: Uuid) -> Result<Vec<UserOrg>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"
        SELECT
            o.id   AS organization_id,
            o.name AS organization_name,
            o.slug AS organization_slug,
            m.role AS role,
            m.is_principal AS is_principal
        FROM membership m
        JOIN organization o ON o.id = m.organization_id
        WHERE m.user_id = $1
        ORDER BY o.name ASC, o.id ASC
        "#,
        user_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let Some(role) = Role::parse(&r.role) else {
                // `role` is text + CHECK, so an unrecognized value can only come
                // from a manual DB edit or a role added to the schema before the
                // enum — skip it, but log, since it silently costs the user a
                // workspace entry rather than failing loudly.
                tracing::warn!(
                    organization_id = %r.organization_id,
                    role = %r.role,
                    "membership has an unrecognized role; omitting from workspace list"
                );
                return None;
            };
            Some(UserOrg {
                organization_id: r.organization_id,
                organization_name: r.organization_name,
                organization_slug: r.organization_slug,
                role,
                is_principal: r.is_principal,
            })
        })
        .collect())
}

/// The usernames of an org's members, alphabetical — the console's assignee
/// autocomplete list.
///
/// One JOIN rather than a `list_for_org` followed by a `user::find_by_id` per
/// row: the list is decoration on a form, and it must not cost a query per
/// member to render.
pub async fn member_usernames(
    pool: &PgPool,
    organization_id: Uuid,
    limit: i64,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT u.username as "username!"
        FROM membership m
        JOIN "user" u ON u.id = m.user_id
        WHERE m.organization_id = $1
        ORDER BY u.username ASC
        LIMIT $2
        "#,
        organization_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trips_through_str() {
        for role in [
            Role::Owner,
            Role::Archivist,
            Role::Conductor,
            Role::Musician,
        ] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
    }

    #[test]
    fn role_parse_rejects_unknown() {
        assert_eq!(Role::parse("superadmin"), None);
    }

    #[test]
    fn principal_subset_check_passes_when_subset() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert!(principal_ids_are_subset(&[a, b], &[a]));
        assert!(principal_ids_are_subset(&[a, b], &[]));
        assert!(principal_ids_are_subset(&[a, b], &[a, b]));
    }

    #[test]
    fn principal_subset_check_fails_when_not_subset() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        assert!(!principal_ids_are_subset(&[a], &[a, b]));
        assert!(!principal_ids_are_subset(&[], &[b]));
    }
}
