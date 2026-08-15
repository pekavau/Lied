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
use crate::domain::{arrangement, collection, collection_item};
use crate::listing::{check_if_match, SortDirection};
use crate::routes::admin::arrangements::{
    audit_ctx, blank_to_none, csrf_token, require_found, scoped_arrangement,
};
use crate::routes::admin::console::{self, ConsoleCtx, Section};
use crate::routes::admin::layout;
use crate::routes::admin::members::{field, multi, parse_ids};
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
        .route(
            "/orgs/:org_id/collections/:collection_id/items",
            post(add_item),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id/items/reorder",
            post(reorder_items),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id/items/:item_id/delete",
            post(delete_item),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id/items/:item_id/undelete",
            post(undelete_item),
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

    let items = match collection_item::list_for_collection(&state.db, collection_id).await {
        Ok(items) => items,
        Err(error) => {
            tracing::error!(%error, "failed to list collection items");
            return error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the collection's pieces.",
            );
        }
    };
    let page_size = i64::from(state.config.max_page_size);
    let (removed, removed_total) =
        collection_item::list_deleted_for_collection(&state.db, collection_id, page_size, 0)
            .await
            .unwrap_or_default();
    // Candidate pieces to add: the org's live arrangements, minus the ones
    // already in this collection (an arrangement may appear once per index, but
    // offering a duplicate by default is a trap rather than a feature).
    let present: std::collections::HashSet<Uuid> =
        items.iter().map(|i| i.item.arrangement_id).collect();
    let candidates = match arrangement::list_for_org(
        &state.db,
        org_id,
        page_size,
        0,
        "title",
        SortDirection::Asc,
        None,
        None,
    )
    .await
    {
        Ok((rows, _)) => rows,
        Err(error) => {
            tracing::error!(%error, "failed to list arrangements for the item picker");
            Vec::new()
        }
    };
    let next_index = items.iter().map(|i| i.item.index).max().unwrap_or(0) + 1;
    let items_base = format!("{}/items", detail_url(org_id, collection_id));

    let body = html! {
        h2 { (c.name) }
        p class="muted" { "Slug " code { (c.slug) } " · " (items.len()) " piece(s)" }

        form method="post" action=(detail_url(org_id, collection_id)) {
            (layout::csrf_field(&token))
            input type="hidden" name="expected_version" value=(c.updated_at.timestamp_millis());
            label { "Name " input type="text" name="name" value=(c.name) required; }
            label { "Type " (type_select(Some(&c.collection_type))) }
            button type="submit" { "Save" }
        }

        h2 { "Pieces" }
        p class="muted" {
            "Ordered by piece number. Use ▲/▼ to reorder — the numbers are "
            "renumbered from 1 on every move."
        }
        // ONE form for the whole table: every row contributes a hidden `order`
        // value (the sequence as rendered), and each ▲/▼ is a submit button
        // naming the row to move. A browser submits only the clicked button, so
        // the handler receives the order plus one directive. The submitted
        // order also acts as a concurrency snapshot — if someone else has since
        // added or removed a piece it is no longer a permutation of the live
        // set, and the domain's reorder refuses it.
        form method="post" action=(format!("{items_base}/reorder")) {
            (layout::csrf_field(&token))
            table {
                thead { tr { th { "#" } th { "Piece" } th { "Order" } th {} } }
                tbody {
                    @for (pos, view) in items.iter().enumerate() {
                        tr {
                            td { (view.item.index) }
                            td {
                                @if view.arrangement_removed {
                                    span class="muted" { "[removed] " (view.arrangement_slug) }
                                } @else {
                                    (view.arrangement_title)
                                    " "
                                    span class="muted" { "(" (view.arrangement_slug) ")" }
                                }
                            }
                            td {
                                input type="hidden" name="order" value=(view.item.id);
                                @if pos > 0 {
                                    button type="submit" name="move"
                                           value=(format!("up:{}", view.item.id)) { "▲" }
                                }
                                @if pos + 1 < items.len() {
                                    button type="submit" name="move"
                                           value=(format!("down:{}", view.item.id)) { "▼" }
                                }
                            }
                            td {
                                a href=(format!("{items_base}/{}/assignments", view.item.id)) {
                                    "Parts"
                                }
                            }
                        }
                    }
                }
            }
        }
        @if items.is_empty() { p class="muted" { "No pieces yet." } }

        // Removing a piece is its own form: it must not ride the reorder form's
        // submit, and a urlencoded POST carries its CSRF token in the body.
        @if !items.is_empty() {
            p class="muted" { "Remove a piece:" }
            @for view in &items {
                form class="inline" method="post"
                     action=(format!("{items_base}/{}/delete", view.item.id)) {
                    (layout::csrf_field(&token))
                    button type="submit" {
                        "Remove " (view.item.index) ". "
                        @if view.arrangement_removed {
                            (view.arrangement_slug)
                        } @else {
                            (view.arrangement_title)
                        }
                    }
                }
                " "
            }
        }

        h2 { "Add a piece" }
        @if candidates.iter().all(|a| present.contains(&a.id)) {
            p class="muted" { "Every arrangement in this organization is already in this collection." }
        } @else {
            form method="post" action=(&items_base) {
                (layout::csrf_field(&token))
                label {
                    "Arrangement "
                    select name="arrangement_id" required {
                        @for a in &candidates {
                            @if !present.contains(&a.id) {
                                option value=(a.id) { (a.title) }
                            }
                        }
                    }
                }
                label {
                    "Piece number "
                    input type="number" name="index" min="1" value=(next_index);
                }
                button type="submit" { "Add piece" }
            }
        }

        @if !removed.is_empty() {
            h2 { "Removed pieces" }
            @if removed_total > removed.len() as i64 {
                p class="muted" {
                    "Showing the " (removed.len()) " most recently removed of " (removed_total) "."
                }
            }
            @for view in &removed {
                form class="inline" method="post"
                     action=(format!("{items_base}/{}/undelete", view.item.id)) {
                    (layout::csrf_field(&token))
                    span { (view.arrangement_title) " " }
                    button type="submit" { "Restore" }
                }
                br;
            }
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

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

/// Apply one `▲`/`▼` directive (`"up:<uuid>"` / `"down:<uuid>"`) to the
/// submitted order by swapping the named item with its neighbour.
///
/// Returns `false` — leaving `order` untouched — for anything that doesn't
/// describe a legal move: a malformed directive, an id that isn't in the
/// submitted order, or a move off either end. The rendered page omits the arrow
/// that would run off the end, but a hand-crafted POST can still ask for it, so
/// this is a guard and not just a UI convenience.
fn apply_move(order: &mut [Uuid], directive: &str) -> bool {
    let Some((direction, raw_id)) = directive.split_once(':') else {
        return false;
    };
    let Ok(id) = Uuid::parse_str(raw_id) else {
        return false;
    };
    let Some(pos) = order.iter().position(|candidate| *candidate == id) else {
        return false;
    };
    let target = match direction {
        "up" if pos > 0 => pos - 1,
        "down" if pos + 1 < order.len() => pos + 1,
        _ => return false,
    };
    order.swap(pos, target);
    true
}

/// Fetch a live item, confirming it belongs to this collection — an item id
/// from another collection (or org) must 404 rather than resolve.
async fn scoped_item(
    state: &AppState,
    collection_id: Uuid,
    item_id: Uuid,
) -> Result<Option<collection_item::CollectionItem>, sqlx::Error> {
    Ok(collection_item::find_by_id(&state.db, item_id)
        .await?
        .filter(|i| i.collection_id == collection_id))
}

async fn scoped_item_including_deleted(
    state: &AppState,
    collection_id: Uuid,
    item_id: Uuid,
) -> Result<Option<collection_item::CollectionItem>, sqlx::Error> {
    Ok(
        collection_item::find_by_id_including_deleted(&state.db, item_id)
            .await?
            .filter(|i| i.collection_id == collection_id),
    )
}

/// Enter, gate, and resolve the collection — the preamble every item write
/// shares.
async fn enter_collection(
    state: &AppState,
    auth: Option<AuthSession>,
    org_id: Uuid,
    collection_id: Uuid,
) -> Result<(ConsoleCtx, collection::Collection), Response> {
    let ctx = enter(state, auth, org_id).await?;
    let c = require_found(
        scoped_collection(state, org_id, collection_id).await,
        &ctx,
        Section::Collections,
        "Collection not found.",
    )?;
    Ok((ctx, c))
}

async fn add_item(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let (ctx, _) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let Some(arrangement_id) =
        field(&pairs, "arrangement_id").and_then(|v| Uuid::parse_str(v).ok())
    else {
        return error_page(&ctx, StatusCode::BAD_REQUEST, "Choose an arrangement.");
    };
    // The arrangement must be this org's — otherwise a forged id would pull a
    // foreign piece into the program (and leak its title through the listing).
    if require_found(
        scoped_arrangement(&state, &ctx, arrangement_id, false).await,
        &ctx,
        Section::Collections,
        "Arrangement not found.",
    )
    .is_err()
    {
        return error_page(&ctx, StatusCode::NOT_FOUND, "Arrangement not found.");
    }

    let index = match field(&pairs, "index")
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(raw) => match raw.parse::<i32>() {
            Ok(n) if n >= 1 => n,
            _ => {
                return error_page(
                    &ctx,
                    StatusCode::BAD_REQUEST,
                    "The piece number must be a positive whole number.",
                )
            }
        },
        None => {
            // Append: one past the highest live index.
            match collection_item::list_for_collection(&state.db, collection_id).await {
                Ok(items) => items.iter().map(|i| i.item.index).max().unwrap_or(0) + 1,
                Err(error) => {
                    tracing::error!(%error, "failed to compute the next piece number");
                    return error_page(
                        &ctx,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Could not add the piece.",
                    );
                }
            }
        }
    };

    match collection_item::create(
        &state.db,
        Uuid::now_v7(),
        collection_id,
        arrangement_id,
        index,
        Some(ctx.user().id),
    )
    .await
    {
        Ok(created) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection_item.create",
                "collection_item",
                Some(created.id),
                serde_json::json!({
                    "collectionId": collection_id,
                    "arrangementId": arrangement_id,
                    "index": index,
                }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        Err(collection_item::CollectionItemError::DuplicateIndex) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "Another piece already holds that number — pick a free one or reorder afterwards.",
        ),
        Err(collection_item::CollectionItemError::UnknownReference) => error_page(
            &ctx,
            StatusCode::NOT_FOUND,
            "That arrangement or collection no longer exists.",
        ),
        Err(error) => {
            tracing::error!(%error, "failed to add a collection item");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not add the piece.",
            )
        }
    }
}

async fn reorder_items(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    // Ordered pairs: one `order` value per row, so the sequence survives.
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let (ctx, _) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let Some(mut order) = parse_ids(&multi(&pairs, "order")) else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "The submitted order was malformed.",
        );
    };
    if order.is_empty() {
        return Redirect::to(&detail_url(org_id, collection_id)).into_response();
    }
    // A `move` directive is the arrow-button path; without one the submitted
    // order is taken as-is (the shape a future drag-and-drop enhancement would
    // post, with no server change needed).
    if let Some(directive) = field(&pairs, "move") {
        if !apply_move(&mut order, directive) {
            return error_page(
                &ctx,
                StatusCode::BAD_REQUEST,
                "That move is not possible — reload the collection and try again.",
            );
        }
    }

    match collection_item::reorder(&state.db, collection_id, &order).await {
        Ok(()) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection_item.reorder",
                "collection",
                Some(collection_id),
                serde_json::json!({ "order": order }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        // The submitted order no longer describes the collection's live items:
        // someone added or removed a piece while this page was open.
        Err(collection_item::CollectionItemError::InvalidReorder) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "This collection changed since the page was loaded, so the order was not saved. \
             Reload and try again.",
        ),
        Err(error) => {
            tracing::error!(%error, "failed to reorder collection items");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the new order.",
            )
        }
    }
}

async fn delete_item(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let (ctx, _) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    if let Err(response) = require_found(
        scoped_item(&state, collection_id, item_id).await,
        &ctx,
        Section::Collections,
        "Piece not found in this collection.",
    ) {
        return response;
    }
    match collection_item::soft_delete(&state.db, item_id).await {
        Ok(_) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection_item.soft_delete",
                "collection_item",
                Some(item_id),
                serde_json::json!({ "collectionId": collection_id }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        Err(error) => {
            tracing::error!(%error, "failed to remove a collection item");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not remove the piece.",
            )
        }
    }
}

async fn undelete_item(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
) -> Response {
    let (ctx, _) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    if let Err(response) = require_found(
        scoped_item_including_deleted(&state, collection_id, item_id).await,
        &ctx,
        Section::Collections,
        "Piece not found in this collection.",
    ) {
        return response;
    }
    match collection_item::undelete(&state.db, item_id).await {
        Ok(_) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection_item.undelete",
                "collection_item",
                Some(item_id),
                serde_json::json!({ "collectionId": collection_id }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        // Its old piece number was taken by another piece while it was gone.
        Err(collection_item::CollectionItemError::DuplicateIndex) => error_page(
            &ctx,
            StatusCode::CONFLICT,
            "Another piece now holds that number — move it first, then restore this one.",
        ),
        Err(error) => {
            tracing::error!(%error, "failed to restore a collection item");
            error_page(
                &ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not restore the piece.",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::apply_move;
    use uuid::Uuid;

    fn ids(n: usize) -> Vec<Uuid> {
        (0..n).map(|_| Uuid::now_v7()).collect()
    }

    #[test]
    fn a_move_swaps_the_named_item_with_its_neighbour() {
        let order = ids(3);
        let mut moved = order.clone();
        assert!(apply_move(&mut moved, &format!("down:{}", order[0])));
        assert_eq!(moved, vec![order[1], order[0], order[2]]);

        let mut moved = order.clone();
        assert!(apply_move(&mut moved, &format!("up:{}", order[2])));
        assert_eq!(moved, vec![order[0], order[2], order[1]]);
    }

    #[test]
    fn an_impossible_or_malformed_move_leaves_the_order_untouched() {
        let order = ids(2);
        for directive in [
            // Off either end: the page hides these arrows, but a crafted POST
            // must not be able to ask for them.
            format!("up:{}", order[0]),
            format!("down:{}", order[1]),
            // An id that isn't in the submitted order at all.
            format!("up:{}", Uuid::now_v7()),
            // Malformed directives.
            format!("sideways:{}", order[0]),
            "up:not-a-uuid".to_string(),
            "nonsense".to_string(),
            String::new(),
        ] {
            let mut candidate = order.clone();
            assert!(
                !apply_move(&mut candidate, &directive),
                "{directive:?} must be refused"
            );
            assert_eq!(candidate, order, "{directive:?} must not reorder anything");
        }
    }
}
