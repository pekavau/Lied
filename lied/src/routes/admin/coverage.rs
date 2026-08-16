//! `/admin/orgs/{org}/coverage` — is the programme ready? (Phase 2, issue #35.)
//!
//! Two read-only screens over `domain::coverage`, the same service `/v1` calls:
//!
//!   * the **dashboard**, every collection with its tallies, worst-covered
//!     first — "which programme needs work" before "which voice";
//!   * the **detail**, per piece and per voice, with the gaps called out.
//!
//! This is the console's one **non-staff** surface. A `musician` with
//! `is_principal` reaches both screens scoped to their own section — the
//! carve-out `Section::Coverage` has been gated for since #30 — and sees no
//! links into the assignment screens, which would 403 for them.

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use maud::{html, Markup};
use uuid::Uuid;

use crate::auth::authz::{coverage_scope_for, CoverageScope};
use crate::auth::extractors::AuthSession;
use crate::domain::collection;
use crate::domain::coverage::{self, CoverageReport};
use crate::error::AppError;
use crate::listing::SortDirection;
use crate::routes::admin::console::{self, ConsoleCtx, Section};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs/:org_id/coverage", get(dashboard))
        .route(
            "/orgs/:org_id/coverage/:collection_id",
            get(collection_coverage),
        )
}

/// Enter the console and resolve *what this caller may see*, which for coverage
/// is inseparable from whether they may see anything at all.
///
/// Staff get the whole programme. A principal gets their own section — and an
/// empty `principal_instrument_ids` yields an empty scope rather than a denial,
/// matching `/v1`: "your section has no instruments configured" is a true
/// answer they can act on.
async fn enter(
    state: &AppState,
    auth: Option<AuthSession>,
    org_id: Uuid,
) -> Result<(ConsoleCtx, coverage::Required), Response> {
    let ctx = console::enter(state, auth, org_id).await?;
    // The same rule `/v1` applies, not a second copy of it: staff see the
    // programme, a principal sees their section, everyone else is refused.
    let scope = match coverage_scope_for(state, ctx.user(), org_id).await {
        Ok(scope) => scope,
        Err(AppError::Forbidden) => return Err(console::section_forbidden(&ctx)),
        Err(error) => {
            // A database failure must not masquerade as an empty section — a
            // principal would read that as "nothing of mine is in this
            // programme", which is a different and much worse answer.
            tracing::error!(%error, "failed to resolve the coverage scope");
            return Err(console::error_page(
                &ctx,
                Section::Coverage,
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Could not work out which coverage you may see. Please retry.",
            ));
        }
    };
    let required = match scope {
        CoverageScope::FullProgram => coverage::Required::EveryVoice,
        CoverageScope::Section(instrument_ids) => coverage::Required::Instruments(instrument_ids),
    };
    Ok((ctx, required))
}

fn is_section_view(required: &coverage::Required) -> bool {
    matches!(required, coverage::Required::Instruments(_))
}

/// Parts and players are different questions with different denominators, so
/// both are named rather than blended into one ratio: a programme can be fully
/// cast and barely acknowledged, or half-cast and fully acknowledged.
fn tally(report: &CoverageReport) -> Markup {
    let conflicts = report.conflicts().count();
    html! {
        @if report.required == 0 {
            span class="muted" { "nothing required" }
        } @else {
            @if report.is_fully_covered() {
                span { (report.covered) "/" (report.required) " parts covered" }
            } @else {
                span class="error" {
                    (report.covered) "/" (report.required) " parts covered — "
                    (report.required - report.covered) " gap(s)"
                }
            }
            " · "
            span { (report.players_acknowledged) " of " (report.players) " players acknowledged" }
            @if conflicts > 0 {
                " · " span class="error" { (conflicts) " conflict(s)" }
            }
        }
    }
}

async fn dashboard(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path(org_id): Path<Uuid>,
) -> Response {
    let (ctx, required) = match enter(&state, auth, org_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };

    let page_size = i64::from(state.config.max_page_size);
    // One query for the whole org, not one per collection: this page exists to
    // be scanned, and a five-way join per row would make it the slowest screen
    // in the console.
    let (collections, reports) = tokio::join!(
        collection::list_for_org(
            &state.db,
            org_id,
            page_size,
            0,
            "name",
            SortDirection::Asc,
            None,
        ),
        coverage::for_org(&state.db, org_id, &required),
    );
    let collections = match collections {
        Ok((rows, _)) => rows,
        Err(error) => {
            tracing::error!(%error, "failed to list collections for coverage");
            return console::error_page(
                &ctx,
                Section::Coverage,
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load coverage.",
            );
        }
    };
    // A dashboard whose whole job is "what still needs attention" must not drop
    // a row it could not compute — a missing collection reads as nothing to
    // worry about.
    let reports = match reports {
        Ok(reports) => reports,
        Err(error) => {
            tracing::error!(%error, "coverage query failed");
            return console::error_page(
                &ctx,
                Section::Coverage,
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Could not work out coverage. The numbers would have been wrong, \
                 so none are shown.",
            );
        }
    };

    let mut reports: Vec<(collection::Collection, CoverageReport)> = collections
        .into_iter()
        .map(|c| {
            let report = reports
                .iter()
                .find(|r| r.collection_id == c.id)
                .cloned()
                .unwrap_or_else(|| CoverageReport {
                    collection_id: c.id,
                    items: Vec::new(),
                    required: 0,
                    covered: 0,
                    players: 0,
                    players_acknowledged: 0,
                });
            (c, report)
        })
        .collect();
    // Worst first: the programme that needs work should not be below the fold.
    // Collections requiring nothing sink to the bottom rather than reading as
    // perfectly covered.
    reports.sort_by(|(_, a), (_, b)| {
        let shortfall = |r: &CoverageReport| {
            if r.required == 0 {
                f64::MAX
            } else {
                r.covered as f64 / r.required as f64
            }
        };
        shortfall(a)
            .partial_cmp(&shortfall(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let body = html! {
        @if is_section_view(&required) {
            p class="muted" {
                "You are seeing your own section — the voices for the instruments "
                "you lead. Staff see the whole programme."
            }
        }
        p class="muted" {
            "A voice is " strong { "assigned" } " once somebody is down to play it, "
            "and " strong { "rehearsed" } " once they have acknowledged the part."
        }
        table {
            thead { tr { th { "Collection" } th { "Type" } th { "Coverage" } } }
            tbody {
                @for (c, report) in &reports {
                    tr {
                        td {
                            a href=(format!("/admin/orgs/{org_id}/coverage/{}", c.id)) { (c.name) }
                        }
                        td { (c.collection_type) }
                        td { (tally(report)) }
                    }
                }
            }
        }
        @if reports.is_empty() {
            p class="muted" { "No collections yet — coverage has nothing to report on." }
        }
    };
    Html(console::console_page(&ctx, Section::Coverage, body).into_string()).into_response()
}

async fn collection_coverage(
    State(state): State<AppState>,
    auth: Option<AuthSession>,
    Path((org_id, collection_id)): Path<(Uuid, Uuid)>,
) -> Response {
    let (ctx, required) = match enter(&state, auth, org_id).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };

    // Scope to the org first: a coverage report names both the programme and
    // the people playing it.
    let found = collection::find_by_id(&state.db, collection_id)
        .await
        .map(|c| c.filter(|c| c.organization_id == org_id));
    let c = match found {
        Ok(Some(c)) => c,
        Ok(None) => {
            return console::error_page(
                &ctx,
                Section::Coverage,
                axum::http::StatusCode::NOT_FOUND,
                "Collection not found.",
            )
        }
        Err(error) => {
            tracing::error!(%error, "failed to load the collection");
            return console::error_page(
                &ctx,
                Section::Coverage,
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load the collection.",
            );
        }
    };

    let report = match coverage::for_collection(&state.db, collection_id, &required).await {
        Ok(report) => report,
        Err(error) => {
            tracing::error!(%error, "coverage query failed");
            return console::error_page(
                &ctx,
                Section::Coverage,
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Could not work out coverage for this collection.",
            );
        }
    };

    // Only staff can act on a gap, so only staff get the link that leads there.
    let can_assign = ctx.can_build_collections();
    let section_view = is_section_view(&required);

    let body = html! {
        h2 { (c.name) " " span class="muted" { "(" (c.collection_type) ")" } }
        p { (tally(&report)) }
        @if section_view {
            p class="muted" {
                "Your section only. A piece with no line below has nothing for "
                "the instruments you lead."
            }
        }

        @for item in &report.items {
            h3 {
                (item.index) ". "
                @if item.arrangement_removed {
                    span class="muted" { "[removed] " (item.arrangement_slug) }
                } @else {
                    (item.arrangement_title)
                }
                " "
                @if item.required == 0 {
                    span class="muted" { "— nothing required" }
                } @else if item.covered == item.required {
                    span class="muted" {
                        "— " (item.covered) "/" (item.required) " parts covered, "
                        (item.players_acknowledged) " of " (item.players)
                        " players acknowledged"
                    }
                } @else {
                    span class="error" {
                        "— " (item.covered) "/" (item.required) " parts covered, "
                        (item.players_acknowledged) " of " (item.players)
                        " players acknowledged"
                    }
                }
                @if can_assign && !item.arrangement_removed {
                    " "
                    a href=(format!(
                        "/admin/orgs/{org_id}/collections/{collection_id}/items/{}/assignments",
                        item.item_id
                    )) { "Assign parts" }
                }
            }
            @if item.arrangement_removed {
                p class="error" {
                    "This piece's arrangement was removed from the catalogue, so it "
                    "has no parts to cover. Restore it or take the piece out of the "
                    "programme."
                }
            }
            // Somebody on two parts of one piece cannot play both at once. Not
            // prevented — sections get juggled by who turns up — but the
            // tallies above would otherwise overstate how ready this is.
            @for conflict in &item.conflicts {
                p class="error" {
                    "⚠ " (conflict.display_name)
                    " (" (conflict.username) ") is on "
                    (conflict.voice_names.join(" and "))
                    " — one player cannot cover both at the same time."
                }
            }
            table {
                thead { tr { th { "Voice" } th { "Players" } th { "Acknowledged" } } }
                tbody {
                    @for voice in &item.voices {
                        tr {
                            td { (voice.voice_name) }
                            td {
                                @if voice.players.is_empty() {
                                    span class="error" { "unassigned" }
                                } @else {
                                    @for (position, player) in voice.players.iter().enumerate() {
                                        @if position > 0 { ", " }
                                        (player.display_name)
                                        @if player.acknowledged_at.is_none()
                                            && player.notified_at.is_none() {
                                            span class="muted" { " (not yet notified)" }
                                        }
                                    }
                                }
                            }
                            td {
                                @if voice.players.is_empty() {
                                    span class="muted" { "—" }
                                } @else {
                                    (voice.acknowledged()) " of " (voice.players.len())
                                }
                            }
                        }
                    }
                }
            }
            @if item.voices.is_empty() && !item.arrangement_removed {
                p class="muted" {
                    @if section_view {
                        "Nothing in this piece for your section."
                    } @else {
                        "This arrangement has no voices yet."
                    }
                }
            }
        }
        @if report.items.is_empty() {
            p class="muted" { "This collection has no pieces yet." }
        }

        p { a href=(format!("/admin/orgs/{org_id}/coverage")) { "← Back to coverage" } }
    };
    Html(console::console_page(&ctx, Section::Coverage, body).into_string()).into_response()
}
