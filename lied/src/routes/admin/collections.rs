//! `/admin/orgs/{org}/collections` — program & standing-collection management
//! for the console (Phase 2, issue #33). HTMX/maud over the phase-1 collection
//! domain (issue #8).
//!
//! Authorization mirrors the permission matrix "Build/edit collections" row =
//! `owner`/`archivist`/`conductor` = [`console::ConsoleCtx::can_build_collections`].
//! This is the section where a conductor has *write* access while arrangements
//! and files stay read-only for them, so every handler gates on
//! `can_build_collections` rather than the `can_edit_arrangements` used by the
//! catalog screens.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::html;
use serde::Deserialize;
use tower_sessions::Session;
use uuid::Uuid;

use crate::auth::extractors::AuthSession;
use crate::domain::audit_log::audit;
use crate::domain::{collection, collection_item};
use crate::listing::{check_if_match, SortDirection};
use crate::routes::admin::arrangements::{audit_ctx, blank_to_none, csrf_token, require_found};
use crate::routes::admin::console::{self, ConsoleCtx, Section};
use crate::routes::admin::layout;
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/orgs/:org_id/collections",
            get(collections_page).post(create_collection),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id",
            get(collection_detail_page).post(update_collection),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id/delete",
            post(delete_collection),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id/undelete",
            post(undelete_collection),
        )
}

#[derive(Deserialize)]
struct CollectionForm {
    name: Option<String>,
    #[serde(rename = "type")]
    collection_type: Option<String>,
    expected_version: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Enter the console and gate on the collection-building capability. Unlike the
/// catalog screens there is no read-only tier here: a role that may see this
/// section may also edit it (musicians never reach it at all).
async fn enter(
    state: &AppState,
    auth: Option<AuthSession>,
    org_id: Uuid,
) -> Result<ConsoleCtx, Response> {
    let ctx = console::enter(state, auth, org_id).await?;
    if !ctx.can_build_collections() {
        return Err(console::section_forbidden(&ctx));
    }
    Ok(ctx)
}

/// Fetch a live collection, confirming it belongs to this org — a collection id
/// from another org must 404, not resolve (the nested-scope lesson from #6).
pub(crate) async fn scoped_collection(
    state: &AppState,
    org_id: Uuid,
    collection_id: Uuid,
) -> Result<Option<collection::Collection>, sqlx::Error> {
    Ok(collection::find_by_id(&state.db, collection_id)
        .await?
        .filter(|c| c.organization_id == org_id))
}

async fn scoped_collection_including_deleted(
    state: &AppState,
    org_id: Uuid,
    collection_id: Uuid,
) -> Result<Option<collection::Collection>, sqlx::Error> {
    Ok(
        collection::find_by_id_including_deleted(&state.db, collection_id)
            .await?
            .filter(|c| c.organization_id == org_id),
    )
}

/// An error page rendered inside the *Collections* section, so the nav keeps
/// highlighting where the user actually is. (`arrangements::error_page` is the
/// same helper hard-wired to the catalog section.)
fn error_page(ctx: &ConsoleCtx, status: StatusCode, message: &str) -> Response {
    console::error_page(ctx, Section::Collections, status, message)
}

/// The console URL of a collection's detail screen.
fn detail_url(org_id: Uuid, collection_id: Uuid) -> String {
    format!("/admin/orgs/{org_id}/collections/{collection_id}")
}

fn list_url(org_id: Uuid) -> String {
    format!("/admin/orgs/{org_id}/collections")
}

/// Validate a submitted collection type against the domain's vocabulary.
fn parse_type(raw: Option<String>) -> Option<String> {
    let value = blank_to_none(raw)?;
    collection::is_valid_type(&value).then_some(value)
}

/// A `<select>` over the collection-type vocabulary, pre-selecting `current`.
fn type_select(current: Option<&str>) -> maud::Markup {
    html! {
        select name="type" {
            @for t in collection::COLLECTION_TYPES {
                option value=(t) selected[current == Some(*t)] { (t) }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// List screen
// ---------------------------------------------------------------------------

async fn collections_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    session: Session,
) -> Response {
    let ctx = match enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let token = match csrf_token(&ctx, &session).await {
        Ok(token) => token,
        Err(response) => return response,
    };

    let page_size = i64::from(state.config.max_page_size);
    let (collections, total) = match collection::list_for_org(
        &state.db,
        org_id,
        page_size,
        0,
        "name",
        SortDirection::Asc,
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "failed to list collections");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load collections.",
            );
        }
    };
    let (deleted, deleted_total) =
        collection::list_deleted_for_org(&state.db, org_id, page_size, 0)
            .await
            .unwrap_or_default();

    let body = html! {
        p class="muted" {
            "A "
            strong { "program" }
            " is a concert in fixed order; a "
            strong { "standing" }
            " collection is drawn from by piece number as needed. Both are indexed."
        }
        table {
            thead { tr { th { "Name" } th { "Type" } th { "Slug" } th {} } }
            tbody {
                @for c in &collections {
                    tr {
                        td { a href=(detail_url(org_id, c.id)) { (c.name) } }
                        td { (c.collection_type) }
                        td class="muted" { (c.slug) }
                        td {
                            form class="inline" method="post"
                                 action=(format!("{}/delete", detail_url(org_id, c.id))) {
                                (layout::csrf_field(&token))
                                button type="submit" { "Delete" }
                            }
                        }
                    }
                }
            }
        }
        @if collections.is_empty() { p class="muted" { "No collections yet." } }
        @if total > collections.len() as i64 {
            p class="muted" {
                "Showing the first " (collections.len()) " of " (total)
                " collections (the server's page limit)."
            }
        }

        h2 { "Create a collection" }
        form method="post" action=(list_url(org_id)) {
            (layout::csrf_field(&token))
            label { "Name " input type="text" name="name" required; }
            label { "Type " (type_select(None)) }
            button type="submit" { "Create collection" }
        }
        p class="muted" {
            "The slug is generated from the name and never changes — renaming the "
            "collection leaves its WebDAV path stable."
        }

        @if !deleted.is_empty() {
            h2 { "Recently deleted" }
            @if deleted_total > deleted.len() as i64 {
                p class="muted" {
                    "Showing the " (deleted.len()) " most recently deleted of " (deleted_total) "."
                }
            }
            @for c in &deleted {
                form class="inline" method="post"
                     action=(format!("{}/undelete", detail_url(org_id, c.id))) {
                    (layout::csrf_field(&token))
                    span { (c.name) " " span class="muted" { "(" (c.collection_type) ")" } " " }
                    button type="submit" { "Restore" }
                }
                br;
            }
        }
    };
    Html(console::console_page(&ctx, Section::Collections, body).into_string()).into_response()
}

// ---------------------------------------------------------------------------
// Detail screen
// ---------------------------------------------------------------------------

async fn collection_detail_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    session: Session,
) -> Response {
    let ctx = match enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let token = match csrf_token(&ctx, &session).await {
        Ok(token) => token,
        Err(response) => return response,
    };
    let c = match require_found(
        scoped_collection(&state, org_id, collection_id).await,
        &ctx,
        Section::Collections,
        "Collection not found.",
    ) {
        Ok(c) => c,
        Err(response) => return response,
    };

    // Items are rendered by the item screen (slice 2); the count is shown here
    // so the detail page is honest about what deleting would hide.
    let item_count = collection_item::list_for_collection(&state.db, collection_id)
        .await
        .map(|items| items.len())
        .unwrap_or(0);

    let body = html! {
        h2 { (c.name) }
        p class="muted" { "Slug " code { (c.slug) } " · " (item_count) " piece(s)" }

        form method="post" action=(detail_url(org_id, collection_id)) {
            (layout::csrf_field(&token))
            input type="hidden" name="expected_version" value=(c.updated_at.timestamp_millis());
            label { "Name " input type="text" name="name" value=(c.name) required; }
            label { "Type " (type_select(Some(&c.collection_type))) }
            button type="submit" { "Save" }
        }

        form class="inline" method="post"
             action=(format!("{}/delete", detail_url(org_id, collection_id))) {
            (layout::csrf_field(&token))
            button type="submit" { "Delete collection" }
        }

        p { a href=(list_url(org_id)) { "← Back to collections" } }
    };
    Html(console::console_page(&ctx, Section::Collections, body).into_string()).into_response()
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

async fn create_collection(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    RequestId(request_id): RequestId,
    Form(form): Form<CollectionForm>,
) -> Response {
    let ctx = match enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let Some(name) = blank_to_none(form.name) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Collection name must not be empty.",
        );
    };
    let Some(collection_type) = parse_type(form.collection_type) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Choose a collection type: program or standing.",
        );
    };
    // NOTE: the shared `slugify` never yields an empty string — a name with no
    // ASCII alphanumerics falls back to the literal "user" (see
    // `domain::user::slugify`, which every entity delegates to). That fallback
    // reads oddly on a collection, but it is pre-existing behaviour shared with
    // orgs and arrangements, so it is not re-specified here; issue #42 is the
    // place where slug generation gets revisited.
    let slug = collection::slugify(&name);

    match collection::create(
        &state.db,
        Uuid::now_v7(),
        org_id,
        &name,
        &slug,
        &collection_type,
        Some(ctx.user().id),
    )
    .await
    {
        Ok(created) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection.create",
                "collection",
                Some(created.id),
                serde_json::json!({
                    "name": created.name,
                    "slug": created.slug,
                    "type": created.collection_type,
                }),
            )
            .await;
            Redirect::to(&detail_url(org_id, created.id)).into_response()
        }
        Err(collection::CollectionError::DuplicateSlug) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "A collection with this slug already exists in this organization.",
        ),
        Err(collection::CollectionError::Database(error)) => {
            tracing::error!(%error, "failed to create collection");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not create the collection.",
            )
        }
    }
}

async fn update_collection(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(form): Form<CollectionForm>,
) -> Response {
    let ctx = match enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let current = match require_found(
        scoped_collection(&state, org_id, collection_id).await,
        &ctx,
        Section::Collections,
        "Collection not found.",
    ) {
        Ok(c) => c,
        Err(response) => return response,
    };
    if check_if_match(form.expected_version.as_deref(), current.updated_at).is_err() {
        return console::precondition_page(
            &ctx,
            Section::Collections,
            "collection",
            &detail_url(org_id, collection_id),
        );
    }
    let Some(name) = blank_to_none(form.name) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Collection name must not be empty.",
        );
    };
    let Some(collection_type) = parse_type(form.collection_type) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Choose a collection type: program or standing.",
        );
    };

    // The slug is immutable by design (CLAUDE.md: renaming a slug is a separate
    // admin operation), so only name and type move here.
    match collection::update(&state.db, collection_id, &name, &collection_type).await {
        Ok(Some(_)) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection.update",
                "collection",
                Some(collection_id),
                serde_json::json!({ "name": name, "type": collection_type }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        Ok(None) => error_page(&ctx, StatusCode::NOT_FOUND, "Collection not found."),
        Err(collection::CollectionError::DuplicateSlug) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "A collection with this slug already exists in this organization.",
        ),
        Err(collection::CollectionError::Database(error)) => {
            tracing::error!(%error, "failed to update collection");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the collection.",
            )
        }
    }
}

async fn delete_collection(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let ctx = match enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = require_found(
        scoped_collection(&state, org_id, collection_id).await,
        &ctx,
        Section::Collections,
        "Collection not found.",
    ) {
        return response;
    }
    match collection::soft_delete(&state.db, collection_id).await {
        Ok(true) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection.soft_delete",
                "collection",
                Some(collection_id),
                serde_json::json!({}),
            )
            .await;
        }
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, "failed to soft-delete collection");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not delete the collection.",
            );
        }
    }
    Redirect::to(&list_url(org_id)).into_response()
}

async fn undelete_collection(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let ctx = match enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    // Scope against the deleted row so a foreign id still 404s rather than
    // being restored into this org's list.
    if let Err(response) = require_found(
        scoped_collection_including_deleted(&state, org_id, collection_id).await,
        &ctx,
        Section::Collections,
        "Collection not found.",
    ) {
        return response;
    }
    match collection::undelete(&state.db, collection_id).await {
        Ok(_) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection.undelete",
                "collection",
                Some(collection_id),
                serde_json::json!({}),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        Err(collection::CollectionError::DuplicateSlug) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "A live collection already holds that slug — rename it before restoring this one.",
        ),
        Err(collection::CollectionError::Database(error)) => {
            tracing::error!(%error, "failed to undelete collection");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not restore the collection.",
            )
        }
    }
}
