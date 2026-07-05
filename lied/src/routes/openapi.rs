//! OpenAPI base document: `ApiDoc`, `SecurityAddon`, shared error schemas
//! and reusable `IntoResponses` error-profile types.
//!
//! The base `ApiDoc` seeds the `OpenApiRouter` tree in `routes/mod.rs`.
//! Every `/v1` handler carries `#[utoipa::path]`; the `routes!` macro
//! registers each handler into both axum and the OpenAPI document so a
//! missing annotation is a compile error, not a documentation gap (§2 of the
//! API guidelines).

use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::Modify;

// ── Security ────────────────────────────────────────────────────────────────

/// Injects the two `/v1` auth schemes into the OpenAPI document so the
/// RapiDoc UI shows an "Authorize" field for both bearer tokens and session
/// cookies.
///
/// - `bearer`: HTTP Bearer (JWT minted by `POST /v1/tokens`)
/// - `session`: ApiKey cookie `id` (set by `/admin/login`)
pub struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let c = openapi.components.get_or_insert_with(Default::default);
        c.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .build(),
            ),
        );
        c.add_security_scheme(
            "session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new("id"))),
        );
    }
}

// ── Shared error body ────────────────────────────────────────────────────────

/// RFC 7807 Problem Details — the body of every `/v1` error response
/// (`application/problem+json`).
///
/// This is a documentation-only schema twin of the serialization produced by
/// `AppError::into_response`; it does **not** replace the production error
/// type in `lied/src/error.rs`.
#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct ProblemDetails {
    /// Stable error-type URI, e.g. `https://lied/errors/not-found`.
    #[schema(rename = "type", example = "https://lied/errors/not-found")]
    pub type_uri: String,
    /// Short human-readable summary of the error kind.
    pub title: String,
    /// HTTP status code (mirrors the response status).
    pub status: u16,
    /// Human-readable detail about this specific occurrence.
    pub detail: String,
    /// Request-ID URI for log correlation.
    pub instance: String,
    /// Field-level validation errors (`garde` failures), present on `400`.
    ///
    /// Shape: `{ "fieldName": ["message1", …] }`.
    #[schema(nullable)]
    pub errors: Option<serde_json::Value>,
}

// ── IntoResponses error-profile types ───────────────────────────────────────
//
// Define once; compose in each handler's `responses(...)` to avoid
// per-handler copy-paste (API guidelines §4.2).

/// `401` + `429` + `500` — every authenticated endpoint can emit these.
#[derive(utoipa::IntoResponses)]
pub enum CommonErrors {
    /// No valid session cookie or bearer token.
    #[response(status = 401, description = "Authentication required")]
    Unauthorized(ProblemDetails),
    /// Rate limit exceeded (login bucket or per-identity ceiling).
    #[response(status = 429, description = "Rate limit exceeded")]
    TooManyRequests(ProblemDetails),
    /// Unhandled DB/storage/unexpected error.
    #[response(status = 500, description = "Internal server error")]
    Internal(ProblemDetails),
}

/// `403 Forbidden` — authenticated but role/ownership forbids the action.
#[derive(utoipa::IntoResponses)]
pub enum Forbidden403 {
    #[response(status = 403, description = "Insufficient permissions")]
    Forbidden(ProblemDetails),
}

/// `404 Not Found` — target absent, soft-deleted, or in a different org.
#[derive(utoipa::IntoResponses)]
pub enum NotFound404 {
    #[response(status = 404, description = "Resource not found")]
    NotFound(ProblemDetails),
}

/// `400 Bad Request` — validation failure or bad sort/filter/pagination param.
#[derive(utoipa::IntoResponses)]
pub enum Validation400 {
    #[response(status = 400, description = "Validation failed")]
    BadRequest(ProblemDetails),
}

/// `409 Conflict` — unique-constraint clash, last-owner invariant, etc.
#[derive(utoipa::IntoResponses)]
pub enum Conflict409 {
    #[response(status = 409, description = "Conflict with existing state")]
    Conflict(ProblemDetails),
}

/// `412 Precondition Failed` — `If-Match` header missing or stale.
#[derive(utoipa::IntoResponses)]
pub enum Precondition412 {
    #[response(status = 412, description = "ETag precondition failed")]
    PreconditionFailed(ProblemDetails),
}

/// `413 Payload Too Large` — upload exceeds `LIED_MAX_UPLOAD_BYTES`.
#[derive(utoipa::IntoResponses)]
pub enum PayloadTooLarge413 {
    #[response(status = 413, description = "Upload exceeds the configured size limit")]
    PayloadTooLarge(ProblemDetails),
}

/// `415 Unsupported Media Type` — MIME type not in the format table.
#[derive(utoipa::IntoResponses)]
pub enum UnsupportedMedia415 {
    #[response(status = 415, description = "Unsupported or unrecognized MIME type")]
    UnsupportedMediaType(ProblemDetails),
}

/// `416 Range Not Satisfiable` — `Range` header cannot be satisfied.
#[derive(utoipa::IntoResponses)]
pub enum Range416 {
    #[response(status = 416, description = "Range not satisfiable")]
    RangeNotSatisfiable(ProblemDetails),
}

// ── Base ApiDoc ──────────────────────────────────────────────────────────────

/// Base OpenAPI document: `info`, tags (one per resource group), security
/// schemes (via `SecurityAddon`), and the shared `ProblemDetails` component
/// schema.
///
/// This is the seed passed to `OpenApiRouter::with_openapi(ApiDoc::openapi())`
/// in `routes/mod.rs`; the per-handler `#[utoipa::path]` annotations and the
/// `routes!` macro then fill in the paths automatically.
#[derive(utoipa::OpenApi)]
#[openapi(
    info(
        title = "Lied API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Sheet-music management REST API. Authenticate with a bearer token \
                       from `POST /v1/tokens` or a session cookie from `/admin/login`.",
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "meta",          description = "API metadata"),
        (name = "instruments",   description = "Instrument controlled vocabulary (instance-wide)"),
        (name = "auth",          description = "Bearer-token minting"),
        (name = "app-passwords", description = "WebDAV app-password credentials"),
        (name = "organizations", description = "Organizations"),
        (name = "users",         description = "User accounts (system-admin)"),
        (name = "members",       description = "Organization memberships"),
        (name = "works",         description = "Abstract musical works (instance-wide)"),
        (name = "arrangements",  description = "Arrangements / editions (org-scoped)"),
        (name = "voices",        description = "Instrument parts within an arrangement"),
        (name = "tags",          description = "Per-org open-ended classifiers"),
        (name = "files",         description = "Score and voice files (MinIO-backed, streamed)"),
        (name = "collections",   description = "Programs & standing repertoire (indexed sets of arrangements)"),
    ),
    components(schemas(ProblemDetails)),
)]
pub struct ApiDoc;
