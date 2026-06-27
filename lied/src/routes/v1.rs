//! `/v1/...` — JSON REST tree. Session cookie or bearer token auth.
//!
//! `GET /v1/instruments` is gated behind [`BearerOrSession`] as of issue #4
//! (the `TODO(#4)` that used to mark it open is resolved here). This item
//! also adds the bearer-token mint endpoint (`POST /v1/tokens`) and the
//! app-password CRUD endpoints, scoped to the authenticated user. Other
//! domain entities land in later phase-1 items.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::auth::app_password::generate as generate_app_password;
use crate::auth::extractors::BearerOrSession;
use crate::auth::password::{verify_dummy, verify_password};
use crate::auth::ratelimit::login_rate_limit_layer;
use crate::domain::audit_log::{audit, AuditContext};
use crate::domain::user;
use crate::domain::{app_password, instrument};
use crate::error::AppError;
use crate::pagination::{Page, PageParams};
use crate::routes::openapi::{CommonErrors, Conflict409, NotFound404};
use crate::routes::RequestId;
use crate::state::AppState;

// ── API version ─────────────────────────────────────────────────────────────

#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct VersionInfo {
    api_version: &'static str,
}

/// Return the current API version string.
#[utoipa::path(
    get,
    path = "/",
    tag = "meta",
    summary = "API version",
    security(),
    responses(
        (status = 200, description = "Current API version", body = VersionInfo),
    )
)]
async fn version() -> Json<VersionInfo> {
    Json(VersionInfo { api_version: "v1" })
}

// ── Instruments ─────────────────────────────────────────────────────────────

/// List the instance-wide instrument controlled vocabulary, ordered by display name.
#[utoipa::path(
    get,
    path = "/instruments",
    tag = "instruments",
    summary = "List instruments",
    params(
        ("limit"  = Option<u32>, Query, description = "Page size (default 50, max 200)"),
        ("offset" = Option<u32>, Query, description = "Page offset"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "Paginated instrument list",
            body = inline(Page<instrument::Instrument>)),
        CommonErrors,
    )
)]
async fn list_instruments(
    _auth: BearerOrSession,
    State(state): State<AppState>,
    Query(params): Query<PageParams>,
) -> Result<Json<Page<instrument::Instrument>>, AppError> {
    let (limit, offset) =
        params.resolve(state.config.default_page_size, state.config.max_page_size);

    let (items, total) = instrument::list(&state.db, i64::from(limit), i64::from(offset)).await?;

    Ok(Json(Page {
        items,
        total,
        limit,
        offset,
    }))
}

// ── Token minting ────────────────────────────────────────────────────────────

/// Request body for `POST /v1/tokens`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct MintTokenRequest {
    /// Optional active org id to scope the token to (CLAUDE.md JWT claims:
    /// `org`, nullable). Not validated against Membership in this item —
    /// org-scoping enforcement is a later authorization concern; minting
    /// just records the claim.
    org: Option<Uuid>,
    /// Login identifier. Required when no session cookie is present.
    username: Option<String>,
    /// User's password. Required when no session cookie is present.
    password: Option<String>,
}

/// Bearer JWT returned by `POST /v1/tokens`.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct MintTokenResponse {
    token: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// Mint a fresh bearer JWT from an existing session cookie or username/password.
#[utoipa::path(
    post,
    path = "/tokens",
    tag = "auth",
    summary = "Mint a bearer token",
    description = "Authenticate via an existing session cookie (common path: logged into admin UI) \
                   or via `username`+`password` in the body (programmatic path). \
                   The returned token is valid for the configured lifetime (default 30 days).",
    security(),
    request_body = MintTokenRequest,
    responses(
        (status = 200, description = "Bearer JWT minted", body = MintTokenResponse),
        (status = 401, description = "Bad credentials"),
        (status = 429, description = "Login rate limit exceeded"),
    )
)]
async fn mint_token(
    State(state): State<AppState>,
    session: tower_sessions::Session,
    RequestId(request_id): RequestId,
    Json(body): Json<MintTokenRequest>,
) -> Result<Json<MintTokenResponse>, AppError> {
    let request_id = Some(request_id);

    let authenticated_user_id = match crate::auth::session::current_user_id(&session)
        .await
        .ok()
        .flatten()
    {
        Some(id) => Some(id),
        None => match (&body.username, &body.password) {
            (Some(username), Some(password)) => {
                let found = user::find_by_username(&state.db, username)
                    .await
                    .map_err(AppError::from)?;
                // Same constant-time treatment as `/admin/login`: verify a
                // dummy hash when the user is absent / has no local password,
                // so latency doesn't leak account existence (username
                // enumeration). See `auth::password::verify_dummy`.
                let verified = match found.as_ref().and_then(|u| u.password_hash.as_deref()) {
                    Some(hash) => verify_password(password, hash).unwrap_or(false),
                    None => {
                        verify_dummy();
                        false
                    }
                };

                if verified {
                    found.map(|u| u.id)
                } else {
                    audit(
                        &state.db,
                        &AuditContext {
                            actor_user_id: found.as_ref().map(|u| u.id),
                            org_id: None,
                            request_id,
                        },
                        "auth.login_failed",
                        "user",
                        found.as_ref().map(|u| u.id),
                        serde_json::json!({ "username": username, "via": "token_mint" }),
                    )
                    .await;
                    None
                }
            }
            _ => None,
        },
    };

    let Some(user_id) = authenticated_user_id else {
        return Err(AppError::Unauthorized);
    };

    let (token, claims) = state
        .jwt_keyring
        .mint(user_id, body.org)
        .map_err(|_| AppError::Internal(anyhow::anyhow!("failed to mint bearer token")))?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(user_id),
            org_id: body.org,
            request_id,
        },
        "auth.token_minted",
        "user",
        Some(user_id),
        serde_json::json!({ "jti": claims.jti }),
    )
    .await;

    Ok(Json(MintTokenResponse {
        token,
        expires_at: chrono::DateTime::from_timestamp(claims.exp, 0)
            .unwrap_or_else(chrono::Utc::now),
    }))
}

// ── App passwords ────────────────────────────────────────────────────────────

/// Request body for `POST /v1/app-passwords`.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateAppPasswordRequest {
    /// Human-readable label (e.g. "iPad in rehearsal room"). Must be unique
    /// per user.
    name: String,
}

/// Response from `POST /v1/app-passwords` — includes the plaintext token
/// shown exactly once.
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct CreateAppPasswordResponse {
    #[serde(flatten)]
    summary: app_password::AppPasswordSummary,
    /// Plaintext token shown exactly once at creation.
    ///
    /// Format: `lied_<base64url(32 bytes)>`. Store it now — it cannot be
    /// retrieved again.
    token: String,
}

/// Create a new WebDAV app password for the authenticated user.
///
/// Returns the plaintext token exactly once; the server stores only an
/// argon2id hash. Use the token as the password in
/// `Authorization: Basic base64(username:token)` for WebDAV clients.
#[utoipa::path(
    post,
    path = "/app-passwords",
    tag = "app-passwords",
    summary = "Create a WebDAV app password",
    security(("bearer" = []), ("session" = [])),
    request_body = CreateAppPasswordRequest,
    responses(
        (status = 201, description = "App password created; token shown once",
            body = CreateAppPasswordResponse),
        CommonErrors,
        Conflict409,
    )
)]
async fn create_app_password(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Json(body): Json<CreateAppPasswordRequest>,
) -> Result<(StatusCode, Json<CreateAppPasswordResponse>), AppError> {
    let generated = generate_app_password()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("failed to generate app password")))?;

    let id = Uuid::now_v7();
    let summary = app_password::create(
        &state.db,
        id,
        auth.user.id,
        &body.name,
        &generated.hash,
        &generated.prefix,
    )
    .await
    .map_err(|err| match err {
        app_password::AppPasswordError::DuplicateName => AppError::PreconditionFailed,
        app_password::AppPasswordError::Database(e) => AppError::Database(e),
    })?;

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: auth.org,
            request_id: Some(request_id),
        },
        "auth.app_password_created",
        "app_password",
        Some(id),
        serde_json::json!({ "name": body.name, "prefix": summary.prefix }),
    )
    .await;

    Ok((
        StatusCode::CREATED,
        Json(CreateAppPasswordResponse {
            summary,
            token: generated.plaintext,
        }),
    ))
}

/// List the authenticated user's own app passwords (metadata only — no hashes or plaintext).
#[utoipa::path(
    get,
    path = "/app-passwords",
    tag = "app-passwords",
    summary = "List app passwords",
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 200, description = "App password list",
            body = Vec<app_password::AppPasswordSummary>),
        CommonErrors,
    )
)]
async fn list_app_passwords(
    auth: BearerOrSession,
    State(state): State<AppState>,
) -> Result<Json<Vec<app_password::AppPasswordSummary>>, AppError> {
    let items = app_password::list_for_user(&state.db, auth.user.id).await?;
    Ok(Json(items))
}

/// Revoke one of the authenticated user's own app passwords.
///
/// Revoking does not end any web session or invalidate any bearer token —
/// the three auth paths are independent.
#[utoipa::path(
    delete,
    path = "/app-passwords/{id}",
    tag = "app-passwords",
    summary = "Revoke an app password",
    params(
        ("id" = Uuid, Path, description = "App password ID"),
    ),
    security(("bearer" = []), ("session" = [])),
    responses(
        (status = 204, description = "App password revoked"),
        CommonErrors,
        NotFound404,
    )
)]
async fn revoke_app_password(
    auth: BearerOrSession,
    State(state): State<AppState>,
    RequestId(request_id): RequestId,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let revoked = app_password::revoke(&state.db, id, auth.user.id).await?;
    if !revoked {
        return Err(AppError::NotFound);
    }

    audit(
        &state.db,
        &AuditContext {
            actor_user_id: Some(auth.user.id),
            org_id: auth.org,
            request_id: Some(request_id),
        },
        "auth.app_password_revoked",
        "app_password",
        Some(id),
        serde_json::json!({}),
    )
    .await;

    Ok(StatusCode::NO_CONTENT)
}

// ── Router ───────────────────────────────────────────────────────────────────

pub fn router(state: AppState) -> OpenApiRouter<AppState> {
    // The `/v1` tree accepts session-cookie *or* bearer auth (CLAUDE.md
    // "Route-tree boundary"), so it needs the same `SessionManagerLayer` the
    // `/admin` tree carries: without it the bare `Session` extractor in
    // `mint_token` 500s, and `BearerOrSession` can never resolve a session
    // cookie (it would silently fall through to bearer-only). The layer
    // shares the store + cookie name with `/admin`, so a cookie set by
    // `/admin/login` authenticates `/v1` calls too.
    let session_layer =
        crate::auth::session::build_session_layer(state.db.clone(), state.config.secure_cookies);

    // `POST /v1/tokens` runs the same `find_by_username` + `verify_password`
    // as `/admin/login`, so it is the same brute-force surface and gets
    // the same per-IP login rate-limit bucket (CLAUDE.md: login/password
    // endpoints 10/min). Scoped to this route only, not the whole tree.
    let token_router = OpenApiRouter::new()
        .routes(routes!(mint_token))
        .layer(login_rate_limit_layer(state.config.ratelimit_login_per_min));

    OpenApiRouter::new()
        .routes(routes!(version))
        .routes(routes!(list_instruments))
        .merge(token_router)
        .routes(routes!(list_app_passwords, create_app_password))
        .routes(routes!(revoke_app_password))
        // Org/User/Membership management (issue #5) — see `routes::orgs` for
        // the handlers; merged rather than re-declared here so that module
        // owns its own route table end to end.
        .merge(crate::routes::orgs::router())
        // Work/Arrangement/Voice/Tag management (issue #6) — see
        // `routes::arrangements`.
        .merge(crate::routes::arrangements::router())
        // File upload/download streamed to/from MinIO (issue #7) — see
        // `routes::files`.
        .merge(crate::routes::files::router())
        .layer(session_layer)
}
