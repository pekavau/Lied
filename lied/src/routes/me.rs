//! `GET /v1/me`: who the caller is and where they belong (issue #97).
//!
//! A client that has just logged in (cookie or bearer token) knows only what
//! it sent; this is how it learns the user's id, name, system-admin flag and
//! the organizations they can enter. `/v1/me/...` is the home of the
//! caller's own views (the "my parts" list of #98 belongs here too).
//!
//! No `ETag`: the response combines the user row with every membership, so
//! it has no single `updated_at` to version, and nothing is written through
//! this path (an `If-Match` would have nothing to guard).

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::extractors::BearerOrSession;
use crate::domain::access;
use crate::domain::membership::{self, Role};
use crate::domain::user::User;
use crate::error::AppError;
use crate::listing;
use crate::pagination::{Page, PageParams};
use crate::routes::openapi::{CommonErrors, Validation400};
use crate::routes::params::ApiListQuery;
use crate::state::AppState;

/// The organization a membership belongs to, as much as a client needs to
/// show and enter it.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeOrganization {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
}

/// One of the caller's memberships, with its organization.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeMembership {
    /// The membership's id, as under `/v1/orgs/{orgId}/members/{memberId}`.
    pub id: Uuid,
    pub organization: MeOrganization,
    pub role: Role,
    pub instrument_ids: Vec<Uuid>,
    pub is_principal: bool,
    pub principal_instrument_ids: Vec<Uuid>,
}

/// The caller and their memberships.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeResponse {
    pub user: User,
    /// Every organization the caller is a member of, ordered by organization
    /// name; empty for a guest or a system admin without memberships. Not
    /// paginated: one entry per organization the caller belongs to.
    pub memberships: Vec<MeMembership>,
}

/// The authenticated caller and their memberships.
///
/// A system admin reaches every organization, but only those with a
/// membership are listed here; `isSystemAdmin` tells the client the rest.
#[utoipa::path(
    get,
    path = "/me",
    tag = "me",
    summary = "Who am I",
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "The caller and their memberships", body = MeResponse),
        CommonErrors,
    )
)]
async fn get_me(
    auth: BearerOrSession,
    State(state): State<AppState>,
) -> Result<Json<MeResponse>, AppError> {
    let memberships = membership::list_for_user(&state.db, auth.user.id)
        .await?
        .into_iter()
        .map(|m| MeMembership {
            id: m.membership_id,
            organization: MeOrganization {
                id: m.organization_id,
                name: m.organization_name,
                slug: m.organization_slug,
            },
            role: m.role,
            instrument_ids: m.instrument_ids,
            is_principal: m.is_principal,
            principal_instrument_ids: m.principal_instrument_ids,
        })
        .collect();
    Ok(Json(MeResponse {
        user: auth.user,
        memberships,
    }))
}

// ── GET /v1/me/parts (#98) ───────────────────────────────────────────────────

/// The collection a part is played in.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MyPartCollection {
    pub id: Uuid,
    pub name: String,
    /// `program` or `standing`.
    #[serde(rename = "type")]
    pub collection_type: String,
}

/// The piece's place in the collection.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MyPartItem {
    pub id: Uuid,
    /// Running order, 1..n.
    pub position: i32,
    /// The number players are called by (`12`, `12a`).
    pub label: String,
}

/// The arrangement the part belongs to; its summary is
/// `GET /v1/orgs/{orgId}/arrangements/{arrangementId}`.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MyPartArrangement {
    pub id: Uuid,
    pub title: String,
}

/// The voice the caller plays; open it, and its files, under
/// `GET /v1/orgs/{orgId}/arrangements/{arrangementId}/voices/{voiceId}`.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MyPartVoice {
    pub id: Uuid,
    pub name: String,
    /// Primary instrument first.
    pub instrument_ids: Vec<Uuid>,
}

/// One part the caller plays: a live part assignment.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MyPart {
    /// The part assignment's id.
    pub id: Uuid,
    pub organization: MeOrganization,
    pub collection: MyPartCollection,
    pub item: MyPartItem,
    pub arrangement: MyPartArrangement,
    pub voice: MyPartVoice,
    pub notified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub acknowledged_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<access::Part> for MyPart {
    fn from(p: access::Part) -> Self {
        MyPart {
            id: p.assignment_id,
            organization: MeOrganization {
                id: p.organization_id,
                name: p.organization_name,
                slug: p.organization_slug,
            },
            collection: MyPartCollection {
                id: p.collection_id,
                name: p.collection_name,
                collection_type: p.collection_type,
            },
            item: MyPartItem {
                id: p.item_id,
                position: p.item_position,
                label: p.item_label,
            },
            arrangement: MyPartArrangement {
                id: p.arrangement_id,
                title: p.arrangement_title,
            },
            voice: MyPartVoice {
                id: p.voice_id,
                name: p.voice_name,
                instrument_ids: p.voice_instrument_ids,
            },
            notified_at: p.notified_at,
            acknowledged_at: p.acknowledged_at,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PartsQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    sort: Option<String>,
}

/// A filter value that does not parse: a 400 naming the filter.
fn bad_filter(field: &str, expected: &str) -> AppError {
    AppError::field(
        &format!("filter[{field}]"),
        &format!("{field} must be {expected}"),
    )
}

fn part_filters(raw: &listing::RawFilters) -> Result<access::PartFilters, AppError> {
    let filters = listing::resolve_filters(raw, access::PARTS_FILTER_ALLOWLIST)
        .map_err(AppError::Validation)?;
    let mut out = access::PartFilters::default();
    for (field, value) in filters {
        if value.is_empty() {
            continue; // an empty filter is no filter (docs/design.md, Sort and filter)
        }
        match field.as_str() {
            "organizationId" => {
                out.organization_id = Some(
                    Uuid::parse_str(&value)
                        .map_err(|_| bad_filter("organizationId", "an organization id"))?,
                );
            }
            "collectionId" => {
                out.collection_id = Some(
                    Uuid::parse_str(&value)
                        .map_err(|_| bad_filter("collectionId", "a collection id"))?,
                );
            }
            "acknowledged" => {
                out.acknowledged = Some(match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(bad_filter("acknowledged", "`true` or `false`")),
                });
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The caller's parts across all their ensembles.
///
/// Every live part assignment of the caller, in every organization: their
/// own ensembles and those they play in as a guest. Each row names the
/// organization, collection, piece and voice, so a client can open the part
/// (`GET …/arrangements/{arrangementId}/voices/{voiceId}` and its files)
/// without walking every organization. A part disappears when its
/// assignment is removed or its piece, collection, voice or arrangement is
/// deleted.
#[utoipa::path(
    get,
    path = "/me/parts",
    tag = "me",
    summary = "My parts",
    params(
        ("limit"  = Option<u32>,    Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>,    Query, description = "Page offset"),
        ("sort"   = Option<String>, Query,
            description = "Sort as `field`, `field:asc` or `field:desc` (a bare field sorts ascending). Allowed fields: `organization` (default, ascending), `collection`, `position`, `notifiedAt`, `acknowledgedAt`. Ties go by organization, collection and position; unset times sort last."),
        ("filter[organizationId]" = Option<Uuid>, Query, description = "Only parts in this organization"),
        ("filter[collectionId]"   = Option<Uuid>, Query, description = "Only parts in this collection"),
        ("filter[acknowledged]"   = Option<bool>, Query, description = "`true`: only acknowledged parts; `false`: only those not yet acknowledged"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated list of the caller's parts",
            body = inline(Page<MyPart>)),
        CommonErrors,
        Validation400,
    )
)]
async fn list_my_parts(
    auth: BearerOrSession,
    State(state): State<AppState>,
    ApiListQuery {
        params: q,
        filters: raw_filters,
    }: ApiListQuery<PartsQuery>,
) -> Result<Json<Page<MyPart>>, AppError> {
    let (limit, offset) = PageParams {
        limit: q.limit,
        offset: q.offset,
    }
    .resolve(state.config.default_page_size, state.config.max_page_size);
    let (sort_column, sort_direction) = listing::resolve_sort(
        q.sort.as_deref(),
        access::PARTS_SORT_ALLOWLIST,
        access::PARTS_DEFAULT_SORT,
    )
    .map_err(AppError::Validation)?;
    let filters = part_filters(&raw_filters)?;

    let (parts, total) = access::list_parts(
        &state.db,
        auth.user.id,
        filters,
        sort_column,
        sort_direction,
        i64::from(limit),
        i64::from(offset),
    )
    .await?;
    Ok(Json(Page {
        items: parts.into_iter().map(MyPart::from).collect(),
        total,
        limit,
        offset,
    }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_me))
        .routes(routes!(list_my_parts))
}
