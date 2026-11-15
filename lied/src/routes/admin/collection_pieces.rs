//! The pieces of a collection in the console (#104): the piece table with
//! its gaps and section headings, adding by label, relabel and swap, the
//! sections panel and the Renumber preview.
//!
//! Every rule lives in the domain (docs/design.md, *Collection numbering*,
//! *CollectionSection*): this module renders
//! [`collection_item::Numbering::choices`] and posts to the domain's write
//! functions, whose refusal messages it shows as they are. It never decides
//! itself which label is allowed. The only arithmetic here is presentation:
//! which range a free number or a piece is drawn under, and the muted
//! "62–100 free" line between a range's last piece and the next section.
//!
//! The page shell (collection form, arrangement picker) stays in
//! [`super::collections`], which calls the render functions below.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::{html, Markup};
use tower_sessions::Session;
use uuid::Uuid;

use crate::auth::extractors::AuthSession;
use crate::domain::audit_log::audit;
use crate::domain::collection_item::{
    self, CollectionItemError, CollectionItemView, Label, LabelChoices,
};
use crate::domain::collection_section::{self, CollectionSection, SectionError};
use crate::listing;
use crate::routes::admin::arrangements::{audit_ctx, csrf_token, require_found};
use crate::routes::admin::collections::{
    detail_url, enter_collection, error_page, item_error_page, scoped_item,
};
use crate::routes::admin::console::{self, ConsoleCtx, Section};
use crate::routes::admin::layout;
use crate::routes::admin::members::field;
use crate::routes::RequestId;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/orgs/:org_id/collections/:collection_id/items/:item_id/relabel",
            post(relabel_item),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id/items/:item_id/swap",
            post(swap_item),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id/renumber",
            get(renumber_page).post(apply_renumber),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id/sections",
            post(create_section),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id/sections/:section_id",
            post(update_section),
        )
        .route(
            "/orgs/:org_id/collections/:collection_id/sections/:section_id/delete",
            post(delete_section),
        )
}

// ---------------------------------------------------------------------------
// Form fields shared by add, relabel and restore
// ---------------------------------------------------------------------------

/// The label a form asks for: the letter field wins over the select when it
/// is filled in ("or a letter label, e.g. 12a").
pub(super) fn submitted_label(pairs: &[(String, String)]) -> Option<&str> {
    let filled = |key| {
        field(pairs, key)
            .map(str::trim)
            .filter(|v: &&str| !v.is_empty())
    };
    filled("letter_label").or_else(|| filled("label"))
}

/// The letter-label field that sits next to every label select.
fn letter_field() -> Markup {
    html! {
        label class="muted" {
            "or a letter label "
            input type="text" name="letter_label" size="5" placeholder="12a"
                  aria-label="Letter label, e.g. 12a";
        }
    }
}

/// Every label the domain offers a new piece, ascending: the gaps, each
/// range's next number and the overall suggestion.
fn offered_labels(choices: &LabelChoices) -> Vec<Label> {
    let mut labels: Vec<Label> = choices
        .gaps
        .iter()
        .copied()
        .chain(choices.sections.iter().filter_map(|s| s.next))
        .chain(choices.suggested)
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

/// A plain select over [`offered_labels`], for relabel and restore. Its
/// first option is empty, so submitting only the letter field works.
fn label_select(choices: &LabelChoices) -> Markup {
    html! {
        select name="label" aria-label="New number" {
            option value="" { "—" }
            @for label in offered_labels(choices) {
                option value=(label) { (label) }
            }
        }
    }
}

/// The add-piece number select: one group per range (the numbers before the
/// first section, then each section) with its gaps and its next number, the
/// overall suggestion preselected. A full section says so instead.
pub(super) fn add_label_select(choices: &LabelChoices) -> Markup {
    let suggested = choices.suggested;
    let option = |label: Label, note: &str| {
        html! {
            option value=(label) selected[Some(label) == suggested] { (label) " " (note) }
        }
    };
    if choices.sections.is_empty() {
        let labels = offered_labels(choices);
        if labels.is_empty() {
            return html! {
                span class="muted" { "No free number: use a letter label, or renumber. " }
                input type="hidden" name="label" value="";
            };
        }
        return html! {
            select name="label" {
                @for label in labels {
                    @if Some(label) == suggested { (option(label, "(next)")) }
                    @else { (option(label, "(free)")) }
                }
            }
        };
    }
    html! {
        select name="label" {
            @for (i, range) in choices.sections.iter().enumerate() {
                @let end = choices.sections.get(i + 1).map(|r| r.starts_at);
                @let inside = |n: u16| n >= range.starts_at && end.is_none_or(|e| n < e);
                @let group = match &range.section {
                    Some(s) => format!("{} (from {})", s.name, s.starts_at),
                    None => "Before the first section".to_string(),
                };
                optgroup label=(group) {
                    @for gap in choices.gaps.iter().filter(|g| inside(g.number())) {
                        (option(*gap, "(free)"))
                    }
                    @match range.next {
                        Some(next) => { (option(next, "(next)")) }
                        None => { option disabled { "full: use a gap, a letter or Renumber" } }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The piece table
// ---------------------------------------------------------------------------

/// One row of a standing collection's table.
enum Row<'a> {
    Heading(&'a CollectionSection),
    Piece(&'a CollectionItemView),
    /// A gap below its range's highest number.
    Free(Label),
    /// The open numbers between a range's last piece and the next section.
    Open(u16, u16),
}

fn label_of(view: &CollectionItemView) -> Label {
    // The column's CHECK keeps every stored label parseable.
    Label::parse(&view.item.label).unwrap_or(Label::plain(1))
}

/// The rows in number order: each range under its heading, its pieces and
/// gaps interleaved, then one line for the open numbers before the next
/// section.
fn standing_rows<'a>(
    items: &'a [CollectionItemView],
    sections: &'a [CollectionSection],
    gaps: &[Label],
) -> Vec<Row<'a>> {
    let start_of = |s: &CollectionSection| u16::try_from(s.starts_at).unwrap_or(u16::MAX);
    let mut ranges: Vec<(Option<&CollectionSection>, u16)> = Vec::new();
    if sections.first().is_none_or(|s| s.starts_at > 1) {
        ranges.push((None, 1));
    }
    ranges.extend(sections.iter().map(|s| (Some(s), start_of(s))));

    let mut rows = Vec::new();
    for (i, (section, start)) in ranges.iter().enumerate() {
        let end = ranges.get(i + 1).map(|(_, s)| *s);
        let inside = |n: u16| n >= *start && end.is_none_or(|e| n < e);
        if let Some(section) = section {
            rows.push(Row::Heading(section));
        }
        let mut entries: Vec<(Label, Row<'a>)> = items
            .iter()
            .filter(|v| inside(label_of(v).number()))
            .map(|v| (label_of(v), Row::Piece(v)))
            .chain(
                gaps.iter()
                    .filter(|g| inside(g.number()))
                    .map(|g| (*g, Row::Free(*g))),
            )
            .collect();
        entries.sort_by_key(|(label, _)| *label);
        let highest = entries
            .iter()
            .filter(|(_, row)| matches!(row, Row::Piece(_)))
            .map(|(label, _)| label.number())
            .max();
        rows.extend(entries.into_iter().map(|(_, row)| row));
        if let Some(end) = end {
            let from = highest.map_or(*start, |h| h + 1);
            if from < end {
                rows.push(Row::Open(from, end - 1));
            }
        }
    }
    rows
}

fn piece_title(view: &CollectionItemView) -> Markup {
    html! {
        @if view.arrangement_removed {
            span class="muted" { "[removed] " (view.arrangement_slug) }
        } @else {
            (view.arrangement_title) " "
            span class="muted" { "(" (view.arrangement_slug) ")" }
        }
    }
}

/// A standing collection's table: no reorder form, so every row carries its
/// own forms (relabel, swap, remove) — issue #48's row actions.
pub(super) fn standing_table(
    org_id: Uuid,
    collection_id: Uuid,
    token: &str,
    items: &[CollectionItemView],
    sections: &[CollectionSection],
    choices: &LabelChoices,
    move_choices: &std::collections::HashMap<Uuid, LabelChoices>,
) -> Markup {
    let items_base = format!("{}/items", detail_url(org_id, collection_id));
    let rows = standing_rows(items, sections, &choices.gaps);
    html! {
        table {
            thead { tr { th { "#" } th { "Piece" } th { "Number" } th { "Swap" } th {} } }
            tbody {
                @for row in &rows {
                    @match row {
                        Row::Heading(section) => { tr class="section" {
                            th colspan="5" { (section.name) " " span class="muted" { "from " (section.starts_at) } }
                        } }
                        Row::Free(label) => { tr class="muted" {
                            td { (label) } td colspan="4" { "free" }
                        } }
                        Row::Open(from, to) => { tr class="muted" {
                            td colspan="5" {
                                @if from == to { (from) " free" } @else { (from) "–" (to) " free" }
                            }
                        } }
                        Row::Piece(view) => { tr {
                            td { (view.item.label) }
                            td { (piece_title(view)) }
                            td {
                                form class="inline" method="post"
                                     action=(format!("{items_base}/{}/relabel", view.item.id)) {
                                    (layout::csrf_field(token))
                                    input type="hidden" name="expected_version"
                                          value=(view.item.updated_at.timestamp_millis());
                                    (label_select(move_choices.get(&view.item.id).unwrap_or(choices))) " "
                                    input type="text" name="letter_label" size="4" placeholder="12a"
                                          aria-label="Letter label";
                                    button type="submit" { "Move" }
                                }
                            }
                            td {
                                @if items.len() > 1 {
                                    form class="inline" method="post"
                                         action=(format!("{items_base}/{}/swap", view.item.id)) {
                                        (layout::csrf_field(token))
                                        select name="with_item_id" aria-label="Swap with" {
                                            @for other in items.iter().filter(|o| o.item.id != view.item.id) {
                                                option value=(other.item.id) {
                                                    (other.item.label) ". " (other.arrangement_title)
                                                }
                                            }
                                        }
                                        button type="submit" { "Swap" }
                                    }
                                }
                            }
                            td {
                                form class="inline" method="post"
                                     action=(format!("{items_base}/{}/delete", view.item.id)) {
                                    (layout::csrf_field(token))
                                    button type="submit" { "Remove" }
                                }
                                " "
                                a href=(format!("{items_base}/{}/assignments", view.item.id)) { "Parts" }
                            }
                        } }
                    }
                }
            }
        }
    }
}

/// A program's table. ONE form wraps it: every row contributes a hidden
/// `order` value and each ▲/▼ is a submit button naming its row, so the
/// browser posts the order plus one directive, and the order doubles as a
/// concurrency snapshot. Forms cannot nest, so each row's Remove button
/// points at its own form declared after the table (`form="…"`, #48).
pub(super) fn program_table(
    org_id: Uuid,
    collection_id: Uuid,
    token: &str,
    items: &[CollectionItemView],
) -> Markup {
    let items_base = format!("{}/items", detail_url(org_id, collection_id));
    let remove_form = |id: Uuid| format!("remove-{id}");
    html! {
        form method="post" action=(format!("{items_base}/reorder")) {
            (layout::csrf_field(token))
            table {
                thead { tr { th { "#" } th { "Piece" } th { "Order" } th {} } }
                tbody {
                    @for (pos, view) in items.iter().enumerate() {
                        tr {
                            td { (view.item.label) }
                            td { (piece_title(view)) }
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
                                button type="submit" form=(remove_form(view.item.id)) { "Remove" }
                                " "
                                a href=(format!("{items_base}/{}/assignments", view.item.id)) { "Parts" }
                            }
                        }
                    }
                }
            }
        }
        @for view in items {
            form id=(remove_form(view.item.id)) method="post"
                 action=(format!("{items_base}/{}/delete", view.item.id)) {
                (layout::csrf_field(token))
            }
        }
    }
}

/// The removed pieces, each with Restore. In a standing collection whose
/// old number has been taken meanwhile, Restore asks for a number.
pub(super) fn removed_list(
    org_id: Uuid,
    collection_id: Uuid,
    token: &str,
    removed: &[CollectionItemView],
    live: &[CollectionItemView],
    choices: Option<&LabelChoices>,
) -> Markup {
    let items_base = format!("{}/items", detail_url(org_id, collection_id));
    let taken: HashSet<&str> = live.iter().map(|v| v.item.label.as_str()).collect();
    html! {
        @for view in removed {
            form class="inline" method="post"
                 action=(format!("{items_base}/{}/undelete", view.item.id)) {
                (layout::csrf_field(token))
                span { (view.item.label) ". " (view.arrangement_title) " " }
                @if let Some(choices) = choices.filter(|_| taken.contains(view.item.label.as_str())) {
                    span class="muted" { "Its number is taken; restore it as " }
                    (label_select(choices)) " " (letter_field())
                }
                button type="submit" { "Restore" }
            }
            br;
        }
    }
}

/// Sections: add one, change a name or start, delete one. Program pages
/// have none.
pub(super) fn sections_panel(
    org_id: Uuid,
    collection_id: Uuid,
    token: &str,
    sections: &[CollectionSection],
) -> Markup {
    let base = format!("{}/sections", detail_url(org_id, collection_id));
    html! {
        h2 { "Sections" }
        p class="muted" {
            "A section covers the numbers from its start up to the next section's start. "
            "Adding, moving or deleting one changes no piece's number."
        }
        @if !sections.is_empty() {
            table {
                thead { tr { th { "Name" } th { "From" } th {} } }
                tbody {
                    @for section in sections {
                        tr {
                            td colspan="2" {
                                form class="inline" method="post" action=(format!("{base}/{}", section.id)) {
                                    (layout::csrf_field(token))
                                    input type="hidden" name="expected_version"
                                          value=(section.updated_at.timestamp_millis());
                                    input type="text" name="name" value=(section.name) required
                                          aria-label="Section name";
                                    " "
                                    input type="number" name="starts_at" min="1" value=(section.starts_at)
                                          aria-label="First number";
                                    button type="submit" { "Save" }
                                }
                            }
                            td {
                                form class="inline" method="post" action=(format!("{base}/{}/delete", section.id)) {
                                    (layout::csrf_field(token))
                                    input type="hidden" name="expected_version"
                                          value=(section.updated_at.timestamp_millis());
                                    button type="submit" { "Delete" }
                                }
                            }
                        }
                    }
                }
            }
        }
        form method="post" action=(&base) {
            (layout::csrf_field(token))
            label { "Name " input type="text" name="name" required; }
            " "
            label { "From number " input type="number" name="starts_at" min="1" required; }
            button type="submit" { "Add section" }
        }
    }
}

// ---------------------------------------------------------------------------
// Relabel and swap
// ---------------------------------------------------------------------------

async fn relabel_item(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let (ctx, _) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let Some(label) = submitted_label(&pairs) else {
        return error_page(&ctx, StatusCode::BAD_REQUEST, "Choose a number.");
    };
    let expected_version = field(&pairs, "expected_version").and_then(|v| v.parse::<i64>().ok());
    let placement = collection_item::Placement {
        position: None,
        label: Some(label),
    };
    match collection_item::move_item(
        &state.db,
        collection_id,
        item_id,
        expected_version,
        placement,
    )
    .await
    {
        Ok(Some(moved)) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection_item.update",
                "collection_item",
                Some(item_id),
                serde_json::json!({ "collectionId": collection_id, "label": moved.label }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        // Still there means someone moved it first; gone means removed.
        Ok(None) => match scoped_item(&state, collection_id, item_id).await {
            Ok(Some(_)) => console::precondition_page(
                &ctx,
                Section::Collections,
                "piece",
                &detail_url(org_id, collection_id),
            ),
            _ => error_page(
                &ctx,
                StatusCode::NOT_FOUND,
                "Piece not found in this collection.",
            ),
        },
        Err(error) => item_error_page(&ctx, error, "move the piece"),
    }
}

async fn swap_item(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id, item_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let (ctx, _) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let Some(with_item_id) = field(&pairs, "with_item_id").and_then(|v| Uuid::parse_str(v).ok())
    else {
        return error_page(
            &ctx,
            StatusCode::BAD_REQUEST,
            "Choose a piece to swap with.",
        );
    };
    match collection_item::swap(&state.db, collection_id, item_id, with_item_id).await {
        Ok(true) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection_item.swap",
                "collection_item",
                Some(item_id),
                serde_json::json!({ "collectionId": collection_id, "withItemId": with_item_id }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        Ok(false) => error_page(
            &ctx,
            StatusCode::NOT_FOUND,
            "One of the pieces is no longer in this collection.",
        ),
        Err(error) => item_error_page(&ctx, error, "swap the pieces"),
    }
}

// ---------------------------------------------------------------------------
// Renumber
// ---------------------------------------------------------------------------

async fn renumber_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    session: Session,
) -> Response {
    let (ctx, _) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    render_renumber(&state, &ctx, &session, org_id, collection_id, None).await
}

async fn apply_renumber(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    session: Session,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let (ctx, _) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let token = field(&pairs, "preview_token").unwrap_or_default();
    match collection_item::renumber(&state.db, collection_id, token).await {
        Ok(outcome) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection.renumber",
                "collection",
                Some(collection_id),
                serde_json::json!({
                    "changes": outcome.changes,
                    "sectionChanges": outcome.section_changes,
                }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        // Someone changed the collection after the preview: show the
        // current one instead of applying a renumber nobody saw.
        Err(CollectionItemError::StaleRenumber) => {
            render_renumber(
                &state,
                &ctx,
                &session,
                org_id,
                collection_id,
                Some("The collection changed since you opened the preview, so nothing was renumbered. This is the current preview."),
            )
            .await
        }
        Err(error) => item_error_page(&ctx, error, "renumber the collection"),
    }
}

/// The preview: only the pieces and section starts whose number changes,
/// and a confirm form carrying the preview token. `note` explains a
/// re-render after a stale confirm (answered with 409).
async fn render_renumber(
    state: &AppState,
    ctx: &ConsoleCtx,
    session: &Session,
    org_id: Uuid,
    collection_id: Uuid,
    note: Option<&str>,
) -> Response {
    let token = match csrf_token(ctx, session).await {
        Ok(token) => token,
        Err(response) => return response,
    };
    let preview = match collection_item::renumber_preview(&state.db, collection_id).await {
        Ok(preview) => preview,
        Err(error) => return item_error_page(ctx, error, "preview the renumber"),
    };
    let (items, sections) = tokio::join!(
        collection_item::list_for_collection(&state.db, collection_id),
        collection_section::list_for_collection(&state.db, collection_id),
    );
    let titles: HashMap<Uuid, String> = items
        .unwrap_or_default()
        .into_iter()
        .map(|v| (v.item.id, v.arrangement_title))
        .collect();
    let section_names: HashMap<Uuid, String> = sections
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.id, s.name))
        .collect();
    let changes: Vec<_> = preview.changes.iter().filter(|c| c.from != c.to).collect();
    let section_changes: Vec<_> = preview
        .section_changes
        .iter()
        .filter(|c| c.from != c.to)
        .collect();
    let back = detail_url(org_id, collection_id);

    let body = html! {
        h2 { "Renumber" }
        @if let Some(note) = note { p class="error" { (note) } }
        @if changes.is_empty() && section_changes.is_empty() {
            p { "Nothing to renumber: the numbers already run without gaps." }
        } @else {
            p {
                "Renumbering closes every gap and turns letter labels into plain numbers. "
                strong { "The paper folders must be re-sleeved to match" }
                ": every player's copy carries the old numbers."
            }
            @if !changes.is_empty() {
                table {
                    thead { tr { th { "Piece" } th { "Now" } th { "Becomes" } } }
                    tbody {
                        @for change in &changes {
                            tr {
                                td { (titles.get(&change.item_id).map(String::as_str).unwrap_or("?")) }
                                td { (change.from) }
                                td { (change.to) }
                            }
                        }
                    }
                }
            }
            @if !section_changes.is_empty() {
                h3 { "Section starts" }
                table {
                    thead { tr { th { "Section" } th { "Now from" } th { "Becomes from" } } }
                    tbody {
                        @for change in &section_changes {
                            tr {
                                td { (section_names.get(&change.section_id).map(String::as_str).unwrap_or("?")) }
                                td { (change.from) }
                                td { (change.to) }
                            }
                        }
                    }
                }
            }
            form method="post" action=(format!("{back}/renumber")) {
                (layout::csrf_field(&token))
                input type="hidden" name="preview_token" value=(preview.preview_token);
                button type="submit" { "Renumber now" }
            }
        }
        p { a href=(back) { "← Back to the collection" } }
    };
    let status = if note.is_some() {
        StatusCode::CONFLICT
    } else {
        StatusCode::OK
    };
    (
        status,
        Html(console::console_page(ctx, Section::Collections, body).into_string()),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

fn section_error_page(ctx: &ConsoleCtx, err: SectionError) -> Response {
    let status = match &err {
        SectionError::Database(error) => {
            tracing::error!(%error, "collection section write failed");
            return error_page(
                ctx,
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not save the section.",
            );
        }
        SectionError::CollectionGone => StatusCode::NOT_FOUND,
        SectionError::InvalidStart { .. } => StatusCode::BAD_REQUEST,
        SectionError::NotStanding | SectionError::StartTaken { .. } => StatusCode::CONFLICT,
    };
    let mut message = err.to_string().replace("startsAt", "The first number");
    if let Some(first) = message.get(..1) {
        message = first.to_uppercase() + &message[1..];
    }
    error_page(ctx, status, &format!("{message}."))
}

/// The submitted name (non-blank) and start, or a 400 page.
fn section_fields<'a>(
    ctx: &ConsoleCtx,
    pairs: &'a [(String, String)],
) -> Result<(&'a str, i32), Response> {
    let Some(name) = field(pairs, "name")
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return Err(error_page(
            ctx,
            StatusCode::BAD_REQUEST,
            "A section needs a name.",
        ));
    };
    let Some(start) = field(pairs, "starts_at").and_then(|v| v.trim().parse::<i32>().ok()) else {
        return Err(error_page(
            ctx,
            StatusCode::BAD_REQUEST,
            "A section's first number must be a whole number.",
        ));
    };
    Ok((name, start))
}

async fn scoped_section(
    state: &AppState,
    collection_id: Uuid,
    section_id: Uuid,
) -> Result<Option<CollectionSection>, sqlx::Error> {
    Ok(collection_section::find_by_id(&state.db, section_id)
        .await?
        .filter(|s| s.collection_id == collection_id))
}

/// A guarded section write matched nothing: still there means another edit
/// got in first (412); gone means it was deleted meanwhile.
async fn section_missed(
    state: &AppState,
    ctx: &ConsoleCtx,
    org_id: Uuid,
    collection_id: Uuid,
    section_id: Uuid,
) -> Response {
    match scoped_section(state, collection_id, section_id).await {
        Ok(Some(_)) => console::precondition_page(
            ctx,
            Section::Collections,
            "section",
            &detail_url(org_id, collection_id),
        ),
        _ => error_page(ctx, StatusCode::NOT_FOUND, "Section not found."),
    }
}

async fn create_section(
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
    let (name, start) = match section_fields(&ctx, &pairs) {
        Ok(fields) => fields,
        Err(response) => return response,
    };
    match collection_section::create(
        &state.db,
        Uuid::now_v7(),
        collection_id,
        name,
        start,
        Some(ctx.user().id),
    )
    .await
    {
        Ok(created) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection_section.create",
                "collection_section",
                Some(created.id),
                serde_json::json!({
                    "collectionId": collection_id,
                    "name": created.name,
                    "startsAt": created.starts_at,
                }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        Err(error) => section_error_page(&ctx, error),
    }
}

async fn update_section(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id, section_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let (ctx, _) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let current = match require_found(
        scoped_section(&state, collection_id, section_id).await,
        &ctx,
        Section::Collections,
        "Section not found.",
    ) {
        Ok(section) => section,
        Err(response) => return response,
    };
    if listing::check_if_match(field(&pairs, "expected_version"), current.updated_at).is_err() {
        return console::precondition_page(
            &ctx,
            Section::Collections,
            "section",
            &detail_url(org_id, collection_id),
        );
    }
    let (name, start) = match section_fields(&ctx, &pairs) {
        Ok(fields) => fields,
        Err(response) => return response,
    };
    match collection_section::update(
        &state.db,
        collection_id,
        section_id,
        Some(listing::version_of(current.updated_at)),
        collection_section::SectionChanges {
            name: Some(name),
            starts_at: Some(start),
        },
    )
    .await
    {
        Ok(Some(updated)) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection_section.update",
                "collection_section",
                Some(section_id),
                serde_json::json!({
                    "collectionId": collection_id,
                    "name": updated.name,
                    "startsAt": updated.starts_at,
                }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        Ok(None) => section_missed(&state, &ctx, org_id, collection_id, section_id).await,
        Err(error) => section_error_page(&ctx, error),
    }
}

async fn delete_section(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id, section_id)): Path<(Uuid, Uuid, Uuid)>,
    RequestId(request_id): RequestId,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    let (ctx, _) = match enter_collection(&state, auth, org_id, collection_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let current = match require_found(
        scoped_section(&state, collection_id, section_id).await,
        &ctx,
        Section::Collections,
        "Section not found.",
    ) {
        Ok(section) => section,
        Err(response) => return response,
    };
    if listing::check_if_match(field(&pairs, "expected_version"), current.updated_at).is_err() {
        return console::precondition_page(
            &ctx,
            Section::Collections,
            "section",
            &detail_url(org_id, collection_id),
        );
    }
    match collection_section::delete(
        &state.db,
        collection_id,
        section_id,
        Some(listing::version_of(current.updated_at)),
    )
    .await
    {
        Ok(true) => {
            audit(
                &state.db,
                &audit_ctx(&ctx, request_id),
                "collection_section.delete",
                "collection_section",
                Some(section_id),
                serde_json::json!({ "collectionId": collection_id, "name": current.name }),
            )
            .await;
            Redirect::to(&detail_url(org_id, collection_id)).into_response()
        }
        Ok(false) => section_missed(&state, &ctx, org_id, collection_id, section_id).await,
        Err(error) => section_error_page(&ctx, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(text: &str) -> Label {
        Label::parse(text).unwrap()
    }

    #[test]
    fn the_letter_field_wins_over_the_select() {
        let pairs = |label: &str, letter: &str| {
            vec![
                ("label".to_string(), label.to_string()),
                ("letter_label".to_string(), letter.to_string()),
            ]
        };
        assert_eq!(submitted_label(&pairs("4", "12a")), Some("12a"));
        assert_eq!(submitted_label(&pairs("4", "  ")), Some("4"));
        assert_eq!(submitted_label(&pairs("", "")), None);
    }

    #[test]
    fn offered_labels_are_sorted_and_unique() {
        let choices = LabelChoices {
            suggested: Some(l("7")),
            gaps: vec![l("2"), l("5")],
            sections: Vec::new(),
        };
        assert_eq!(offered_labels(&choices), vec![l("2"), l("5"), l("7")]);
    }
}
