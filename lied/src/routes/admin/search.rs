//! `/admin/orgs/{org}/search` — the conductor's repertoire-planning surface
//! (Phase 2, issue #34).
//!
//! A query box plus facets over the same `arrangement::list_for_org` the `/v1`
//! endpoint calls: one search implementation, two presentations. The form is a
//! **GET**, so a search is a URL — it can be bookmarked, shared with the
//! archivist, or kept open in a tab while a program is assembled.
//!
//! Staff-only, matching `Section::Search`'s existing gate: this is planning,
//! not consumption. Musicians reach their parts over WebDAV.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use maud::html;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::extractors::AuthSession;
use crate::domain::{arrangement, instrument, tag};
use crate::listing::SortDirection;
use crate::routes::admin::console::{self, Section};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/orgs/:org_id/search", get(search_page))
}

/// The search form, straight off the query string. Every field is a `String`
/// so a half-typed number renders back into the form instead of 400-ing the
/// page the user is still filling in — the console validates leniently and
/// tells them, where `/v1` rejects.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SearchForm {
    q: String,
    status: String,
    difficulty_min: String,
    difficulty_max: String,
    duration_max_minutes: String,
    tag: Vec<String>,
    instrument_id: String,
    sort: String,
}

impl SearchForm {
    /// Whether the user has actually asked for anything. An untouched form
    /// shows the whole catalogue, which is a reasonable browse.
    fn is_empty(&self) -> bool {
        self.q.trim().is_empty()
            && self.status.is_empty()
            && self.difficulty_min.is_empty()
            && self.difficulty_max.is_empty()
            && self.duration_max_minutes.is_empty()
            && self.tag.is_empty()
            && self.instrument_id.is_empty()
    }
}

/// A field the user typed that could not be understood. Collected rather than
/// fatal: the rest of the search still runs, and the page says which box was
/// ignored — a search that silently drops a facet is the failure mode this
/// screen must not have.
struct Complaint {
    field: &'static str,
    message: String,
}

fn parse_number<T: std::str::FromStr>(
    raw: &str,
    field: &'static str,
    what: &str,
    complaints: &mut Vec<Complaint>,
) -> Option<T> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<T>() {
        Ok(value) => Some(value),
        Err(_) => {
            complaints.push(Complaint {
                field,
                message: format!("'{trimmed}' is not {what} — that filter was ignored."),
            });
            None
        }
    }
}

async fn search_page(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
    Query(form): Query<SearchForm>,
) -> Response {
    let ctx = match console::enter(&state, auth, org_id).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if !ctx.is_staff() {
        return console::section_forbidden(&ctx);
    }

    let page_size = i64::from(state.config.max_page_size);
    let (tags, instruments) = tokio::join!(
        tag::list_for_org(
            &state.db,
            org_id,
            page_size,
            0,
            "name",
            SortDirection::Asc,
            None
        ),
        instrument::list(&state.db, page_size, 0),
    );
    let tags = tags.map(|(rows, _)| rows).unwrap_or_default();
    let instruments = instruments.map(|(rows, _)| rows).unwrap_or_default();

    let mut complaints = Vec::new();
    let tag_ids: Vec<Uuid> = form
        .tag
        .iter()
        .filter(|value| !value.is_empty())
        .filter_map(|value| Uuid::parse_str(value).ok())
        .collect();
    let duration_max_seconds = parse_number::<i32>(
        &form.duration_max_minutes,
        "Longest",
        "a whole number of minutes",
        &mut complaints,
    )
    .map(|minutes| minutes.saturating_mul(60));

    let search = arrangement::ArrangementSearch {
        q: Some(form.q.trim()).filter(|value| !value.is_empty()),
        status: Some(form.status.as_str()).filter(|value| !value.is_empty()),
        difficulty_min: parse_number(
            &form.difficulty_min,
            "Difficulty from",
            "a whole number",
            &mut complaints,
        ),
        difficulty_max: parse_number(
            &form.difficulty_max,
            "Difficulty to",
            "a whole number",
            &mut complaints,
        ),
        duration_min_seconds: None,
        duration_max_seconds,
        tag_ids,
        instrument_id: Uuid::parse_str(form.instrument_id.trim()).ok(),
    };

    let has_query = search.q.is_some();
    let sort = if form.sort.is_empty() && has_query {
        // A search wants its best matches first; a browse wants alphabetical.
        Some("relevance")
    } else {
        Some(form.sort.as_str()).filter(|value| !value.is_empty())
    };
    let (sort_column, sort_direction) =
        match arrangement::resolve_search_sort(sort, has_query, ("a.title", SortDirection::Asc)) {
            Ok(resolved) => resolved,
            Err(_) => ("a.title", SortDirection::Asc),
        };

    let (results, total) = match arrangement::list_for_org(
        &state.db,
        org_id,
        page_size,
        0,
        sort_column,
        sort_direction,
        &search,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "archive search failed");
            return console::error_page(
                &ctx,
                Section::Search,
                StatusCode::INTERNAL_SERVER_ERROR,
                "The search could not be run.",
            );
        }
    };

    let body = html! {
        p class="muted" {
            "Search titles, composers, arrangers, tags and instrumentation. "
            "Spelling does not have to be exact, and accents are optional: "
            "searching " code { "bolero" } " finds " code { "Boléro" } "."
        }

        // GET, so the result is a URL worth keeping.
        form method="get" {
            p {
                input type="search" name="q" value=(form.q) placeholder="e.g. bolero, ravel, festive"
                      size="40";
                " "
                button type="submit" { "Search" }
                " "
                a href=(format!("/admin/orgs/{org_id}/search")) { "Clear" }
            }
            fieldset {
                legend { "Narrow it down" }
                label {
                    "Status "
                    select name="status" {
                        option value="" selected[form.status.is_empty()] { "any" }
                        option value="active" selected[form.status == "active"] { "active" }
                        option value="archived" selected[form.status == "archived"] { "archived" }
                    }
                }
                label {
                    " Difficulty "
                    input type="number" name="difficulty_min" min="1" max="8" size="2"
                          value=(form.difficulty_min) placeholder="from";
                    " to "
                    input type="number" name="difficulty_max" min="1" max="8" size="2"
                          value=(form.difficulty_max) placeholder="to";
                }
                label {
                    " Max minutes "
                    input type="number" name="duration_max_minutes" min="1" size="3"
                          value=(form.duration_max_minutes);
                }
                label {
                    " Instrument "
                    select name="instrument_id" {
                        option value="" { "any" }
                        @for i in &instruments {
                            option value=(i.id) selected[form.instrument_id == i.id.to_string()] {
                                (i.display_name)
                            }
                        }
                    }
                }
                @if !tags.is_empty() {
                    label {
                        " Tags (all selected must match) "
                        select name="tag" multiple size="4" {
                            @for t in &tags {
                                option value=(t.id) selected[form.tag.contains(&t.id.to_string())] {
                                    (t.name)
                                    @if let Some(kind) = &t.kind { " (" (kind) ")" }
                                }
                            }
                        }
                    }
                }
                label {
                    " Sort "
                    select name="sort" {
                        option value="" { @if has_query { "best match" } @else { "title" } }
                        option value="title" selected[form.sort == "title"] { "title" }
                        option value="difficulty" selected[form.sort == "difficulty"] { "difficulty" }
                        option value="durationSeconds" selected[form.sort == "durationSeconds"] {
                            "duration"
                        }
                    }
                }
            }
        }

        @for complaint in &complaints {
            p class="error" { (complaint.field) ": " (complaint.message) }
        }

        h2 {
            @if form.is_empty() { "All arrangements" } @else { "Results" }
            " " span class="muted" { "(" (total) ")" }
        }
        table {
            thead {
                tr {
                    th { "Title" } th { "Difficulty" } th { "Duration" } th { "Status" }
                }
            }
            tbody {
                @for a in &results {
                    tr {
                        td {
                            a href=(format!("/admin/orgs/{org_id}/arrangements/{}", a.id)) {
                                (a.title)
                            }
                        }
                        td { @match a.difficulty { Some(d) => (d.to_string()), None => "—" } }
                        td {
                            @match a.duration_seconds {
                                Some(seconds) => (format!("{}:{:02}", seconds / 60, seconds % 60)),
                                None => "—",
                            }
                        }
                        td { (a.status) }
                    }
                }
            }
        }
        @if results.is_empty() {
            p class="muted" {
                "Nothing matched. Try fewer words, or drop a filter — the search "
                "already tolerates misspellings, so an empty result usually means "
                "the piece is not in the archive."
            }
        }
        @if total > results.len() as i64 {
            p class="muted" {
                "Showing the first " (results.len()) " of " (total)
                " matches (the server's page limit) — narrow the search to see the rest."
            }
        }
    };
    Html(console::console_page(&ctx, Section::Search, body).into_string()).into_response()
}
